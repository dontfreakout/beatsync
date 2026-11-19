//! HTTP to the server and to R2: the shared client, the server's HTTP base URL, track
//! downloads and the web client's 3-step upload.

use crate::connection::{ConnError, install_crypto_provider};
use futures_util::StreamExt;
use reqwest::StatusCode;
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::time::timeout;
use tokio_util::io::ReaderStream;
use url::Url;

#[cfg(test)]
pub(crate) mod test_server;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// A download whose response head or next body bytes take longer fails; so does a JSON
/// request to the server that takes longer in total. Applied per request, not as the
/// client's `read_timeout`: in reqwest 0.13 that timer also runs while the request body is
/// sent, so it would end every upload that takes longer than 30 s.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Sent data that stays unacknowledged this long closes the connection (`TCP_USER_TIMEOUT`):
/// the bound for an upload that stalls.
const SEND_TIMEOUT: Duration = Duration::from_secs(30);
/// File read size for an upload body.
const UPLOAD_CHUNK: usize = 64 * 1024;
const USER_AGENT: &str = concat!("beatsync-streamer/", env!("CARGO_PKG_VERSION"));

#[derive(Debug)]
pub enum HttpError {
    /// Not an http(s) URL or a server path.
    BadUrl(String),
    /// The server answered with this non-2xx status.
    Status(u16),
    /// The body is larger than this limit in bytes.
    TooLarge(u64),
    /// No response head, or no body bytes, for [`READ_TIMEOUT`].
    TimedOut,
    Network(reqwest::Error),
    Io(std::io::Error),
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadUrl(url) => write!(f, "Invalid URL: {url}"),
            Self::Status(code) => match StatusCode::from_u16(*code) {
                Ok(status) => write!(f, "Download failed: {status}"),
                Err(_) => write!(f, "Download failed: {code}"),
            },
            Self::TooLarge(limit) => {
                write!(f, "Track is larger than {} MB", limit / 1_000_000)
            }
            Self::TimedOut => f.write_str("The connection timed out"),
            Self::Network(e) => {
                // reqwest's own text is generic ("error sending request"); the cause says
                // what went wrong.
                write!(f, "{e}")?;
                match std::iter::successors(std::error::Error::source(e), |c| c.source()).last() {
                    Some(cause) => write!(f, ": {cause}"),
                    None => Ok(()),
                }
            }
            Self::Io(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for HttpError {}

impl From<reqwest::Error> for HttpError {
    fn from(e: reqwest::Error) -> Self {
        // A presigned URL is long and carries its signature: keep it out of the text.
        Self::Network(e.without_url())
    }
}

impl From<std::io::Error> for HttpError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

#[derive(Debug)]
pub enum UploadError {
    /// Not one of the audio extensions the web uploader accepts.
    NotAudio,
    /// The server refused a step with this text.
    Refused(String),
    /// A step answered with this non-2xx status and no text.
    Failed(StatusCode),
    /// The file could not be read or a request did not complete.
    Http(HttpError),
}

impl fmt::Display for UploadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAudio => f.write_str("Please select an audio file"),
            Self::Refused(text) => f.write_str(text),
            Self::Failed(status) => write!(f, "Upload failed: {status}"),
            Self::Http(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for UploadError {}

impl From<HttpError> for UploadError {
    fn from(e: HttpError) -> Self {
        Self::Http(e)
    }
}

impl From<reqwest::Error> for UploadError {
    fn from(e: reqwest::Error) -> Self {
        Self::Http(e.into())
    }
}

impl From<std::io::Error> for UploadError {
    fn from(e: std::io::Error) -> Self {
        Self::Http(e.into())
    }
}

/// The process's one HTTP client (one connection pool; a clone shares it). Installs the
/// ring provider as the rustls default first; https trusts the system's CA certificates.
pub fn client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(build_client).clone()
}

fn build_client() -> reqwest::Client {
    install_crypto_provider();
    let builder = || {
        reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .tcp_user_timeout(SEND_TIMEOUT)
            .user_agent(USER_AGENT)
    };
    builder().build().unwrap_or_else(|e| {
        // Only the TLS setup can fail: the system has no CA certificates. Plain http still
        // works; each https request then fails with a certificate error.
        eprintln!(
            "streamer: no CA certificates for https: {}",
            HttpError::from(e)
        );
        builder()
            .tls_certs_only(Vec::new())
            .build()
            .expect("a client without CA certificates builds")
    })
}

/// The server's HTTP origin for its WebSocket URL: `ws://h:8091/ws?x=1` → `http://h:8091/`.
pub fn http_base(server_url: &str) -> Result<Url, ConnError> {
    let mut url = Url::parse(server_url).map_err(|e| ConnError::InvalidUrl(e.to_string()))?;
    let scheme = match url.scheme() {
        "ws" => "http",
        "wss" => "https",
        other => return Err(ConnError::UnsupportedScheme(other.to_string())),
    };
    url.set_scheme(scheme)
        .map_err(|()| ConnError::InvalidUrl(server_url.to_string()))?;
    url.set_path("/");
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

/// The URL to fetch for a queue URL as the server sends it: an absolute http(s) URL as it
/// is; a path (`/audio/a.mp3`, demo mode) on the server, appended to `base` like the web
/// client's `resolveAudioUrl` does.
pub fn resolve_audio_url(base: &Url, url: &str) -> Result<Url, HttpError> {
    let bad = || HttpError::BadUrl(url.to_string());
    if url.starts_with('/') {
        let origin = base.as_str().trim_end_matches('/');
        return Url::parse(&format!("{origin}{url}")).map_err(|_| bad());
    }
    let parsed = Url::parse(url).map_err(|_| bad())?;
    match parsed.scheme() {
        "http" | "https" => Ok(parsed),
        _ => Err(bad()),
    }
}

/// Streams `url` to `dest` and returns the byte count. The body goes to `<dest>.part`, which
/// is renamed to `dest` when complete; on a failure, or when the future is dropped, the part
/// file is removed.
pub async fn download(
    client: &reqwest::Client,
    url: &Url,
    dest: &Path,
    max_bytes: u64,
) -> Result<u64, HttpError> {
    let part = PartFile::for_dest(dest);
    let response = timeout(READ_TIMEOUT, client.get(url.clone()).send())
        .await
        .map_err(|_| HttpError::TimedOut)??;
    let status = response.status();
    if !status.is_success() {
        return Err(HttpError::Status(status.as_u16()));
    }
    if response.content_length().is_some_and(|n| n > max_bytes) {
        return Err(HttpError::TooLarge(max_bytes));
    }
    let mut file = tokio::fs::File::create(&part.0).await?;
    let mut body = response.bytes_stream();
    let mut written = 0u64;
    while let Some(chunk) = timeout(READ_TIMEOUT, body.next())
        .await
        .map_err(|_| HttpError::TimedOut)?
    {
        let chunk = chunk?;
        written += chunk.len() as u64;
        if written > max_bytes {
            return Err(HttpError::TooLarge(max_bytes));
        }
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    drop(file);
    tokio::fs::rename(&part.0, dest).await?;
    Ok(written)
}

/// The temp file of a download, removed on drop (after the rename there is none to remove).
struct PartFile(PathBuf);

impl PartFile {
    fn for_dest(dest: &Path) -> Self {
        let mut name = dest.as_os_str().to_owned();
        name.push(".part");
        Self(name.into())
    }
}

impl Drop for PartFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The upload content type of an audio file by its extension (any case), for the web
/// uploader's accept list; `None` for any other file.
pub fn audio_content_type(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "ogg" => "audio/ogg",
        "webm" => "audio/webm",
        "flac" => "audio/flac",
        _ => return None,
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PresignRequest<'a> {
    room_id: &'a str,
    file_name: &'a str,
    content_type: &'a str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PresignResponse {
    upload_url: String,
    public_url: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CompleteRequest<'a> {
    room_id: &'a str,
    original_name: &'a str,
    public_url: &'a str,
}

/// Uploads an audio file to the room in the web client's 3 steps
/// (`apps/client/src/lib/api.ts`): a presigned R2 URL from the server, a PUT of the file to
/// it, then `upload/complete`, after which the server adds the track to the queue.
pub async fn upload_audio(
    client: &reqwest::Client,
    base: &Url,
    room_id: &str,
    path: &Path,
) -> Result<(), UploadError> {
    let content_type = audio_content_type(path).ok_or(UploadError::NotAudio)?;
    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
    let file = tokio::fs::File::open(path).await?;
    let len = file.metadata().await?.len();

    let presign = client
        .post(endpoint(base, "upload/get-presigned-url")?)
        .timeout(READ_TIMEOUT)
        .json(&PresignRequest {
            room_id,
            file_name: &file_name,
            content_type,
        })
        .send()
        .await?;
    let PresignResponse {
        upload_url,
        public_url,
    } = accepted(presign).await?.json().await?;

    let upload_url = Url::parse(&upload_url).map_err(|_| HttpError::BadUrl(upload_url))?;
    let put = client
        .put(upload_url)
        // Exactly the type the URL was signed for.
        .header(CONTENT_TYPE, content_type)
        // Without a length the body goes chunked, which a presigned R2 PUT refuses.
        .header(CONTENT_LENGTH, len)
        .body(reqwest::Body::wrap_stream(ReaderStream::with_capacity(
            file,
            UPLOAD_CHUNK,
        )))
        .send()
        .await?;
    if !put.status().is_success() {
        return Err(UploadError::Failed(put.status()));
    }

    let complete = client
        .post(endpoint(base, "upload/complete")?)
        .timeout(READ_TIMEOUT)
        .json(&CompleteRequest {
            room_id,
            original_name: &file_name,
            public_url: &public_url,
        })
        .send()
        .await?;
    accepted(complete).await?;
    Ok(())
}

fn endpoint(base: &Url, path: &str) -> Result<Url, HttpError> {
    base.join(path)
        .map_err(|_| HttpError::BadUrl(path.to_string()))
}

/// The response when the server accepted the step; else its error text (text/plain, as the
/// server's `errorResponse` sends it) or, without one, the status.
async fn accepted(response: reqwest::Response) -> Result<reqwest::Response, UploadError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let is_text = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/plain"));
    let text = if is_text {
        response.text().await.unwrap_or_default()
    } else {
        String::new()
    };
    Err(match text.trim() {
        "" => UploadError::Failed(status),
        text => UploadError::Refused(text.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::test_server::{Reply, Request, TestServer};
    use super::*;
    use crate::connection::ConnError;
    use crate::test_util::TempDir;
    use serde_json::json;
    use std::path::PathBuf;

    fn body(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn http_base_drops_the_ws_path_and_query() {
        let base = http_base("ws://h:8091/ws?x=1").unwrap();
        assert_eq!(base.as_str(), "http://h:8091/");
        assert_eq!(http_base("wss://x/ws").unwrap().as_str(), "https://x/");
        assert_eq!(
            http_base("ftp://x"),
            Err(ConnError::UnsupportedScheme("ftp".to_string()))
        );
        assert!(matches!(
            http_base("not a url"),
            Err(ConnError::InvalidUrl(_))
        ));
    }

    #[test]
    fn resolve_audio_url_joins_paths_to_the_base() {
        let base = http_base("ws://h:8091/ws").unwrap();
        let resolved = resolve_audio_url(&base, "/audio/a.mp3").unwrap();
        assert_eq!(resolved.as_str(), "http://h:8091/audio/a.mp3");
        let r2 = "https://r2.example.com/room-123456/x.mp3";
        assert_eq!(resolve_audio_url(&base, r2).unwrap().as_str(), r2);
        assert!(resolve_audio_url(&base, "a.mp3").is_err());
        assert!(resolve_audio_url(&base, "file:///etc/passwd").is_err());
    }

    #[tokio::test]
    async fn download_writes_the_body_and_leaves_no_temp_file() {
        let data = body(100_000);
        let served = data.clone();
        let server = TestServer::start(move |_| Reply::bytes(served.clone())).await;
        let dir = TempDir::new("download");
        let dest = dir.0.join("track.mp3");

        let n = download(&client(), &server.url("/a.mp3"), &dest, 1_000_000)
            .await
            .unwrap();

        assert_eq!(n, data.len() as u64);
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        assert_eq!(dir.files(), ["track.mp3"]);
    }

    #[tokio::test]
    async fn download_refuses_a_too_large_body() {
        // K4: a track over the limit is not stored, and the error names the limit.
        let dir = TempDir::new("too-large");
        let dest = dir.0.join("track.mp3");
        let announced = TestServer::start(|_| Reply::bytes(body(2_000))).await;
        let err = download(&client(), &announced.url("/a.mp3"), &dest, 1_000)
            .await
            .unwrap_err();
        assert!(matches!(err, HttpError::TooLarge(1_000)), "{err:?}");
        assert!(dir.files().is_empty(), "{:?}", dir.files());

        let unannounced =
            TestServer::start(|_| Reply::bytes(body(2_000)).without_content_length()).await;
        let err = download(&client(), &unannounced.url("/a.mp3"), &dest, 1_000)
            .await
            .unwrap_err();
        assert!(matches!(err, HttpError::TooLarge(1_000)), "{err:?}");
        assert!(dir.files().is_empty(), "{:?}", dir.files());

        assert_eq!(
            HttpError::TooLarge(200_000_000).to_string(),
            "Track is larger than 200 MB"
        );
    }

    #[tokio::test]
    async fn download_reports_a_404() {
        let server = TestServer::start(|_| Reply::text(404, "Not found")).await;
        let dir = TempDir::new("404");
        let err = download(
            &client(),
            &server.url("/gone.mp3"),
            &dir.0.join("t.mp3"),
            1_000,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, HttpError::Status(404)), "{err:?}");
        assert!(dir.files().is_empty(), "{:?}", dir.files());
    }

    fn write_file(dir: &TempDir, name: &str, data: &[u8]) -> PathBuf {
        let path = dir.0.join(name);
        std::fs::write(&path, data).unwrap();
        path
    }

    /// The fake server's step 1 answer: a signed URL on the same server.
    fn presigned(host: &str) -> Reply {
        Reply::json(json!({
            "uploadUrl": format!("http://{host}/r2/room-123456/song.mp3?X-Amz-Signature=abc"),
            "publicUrl": "https://pub.example/room-123456/song.mp3",
        }))
    }

    fn upload_server_reply(put: fn() -> Reply) -> impl Fn(&Request) -> Reply {
        move |req| match (req.method.as_str(), req.target.as_str()) {
            ("POST", "/upload/get-presigned-url") => presigned(req.header("host").unwrap()),
            ("PUT", _) => put(),
            ("POST", "/upload/complete") => Reply::json(json!({ "success": true })),
            _ => Reply::status(404),
        }
    }

    #[tokio::test]
    async fn upload_runs_the_three_steps() {
        let server = TestServer::start(upload_server_reply(|| Reply::status(200))).await;
        let dir = TempDir::new("upload");
        // Larger than one read chunk, so the PUT body arrives in several pieces.
        let data = body(150_000);
        let path = write_file(&dir, "Song.MP3", &data);

        let result = upload_audio(&client(), &server.url("/"), "123456", &path).await;

        let requests = server.requests();
        assert_eq!(requests.len(), 3, "{requests:?}");
        let (sign, put, complete) = (&requests[0], &requests[1], &requests[2]);
        assert_eq!(
            (sign.method.as_str(), sign.target.as_str()),
            ("POST", "/upload/get-presigned-url")
        );
        assert_eq!(
            sign.json(),
            json!({ "roomId": "123456", "fileName": "Song.MP3", "contentType": "audio/mpeg" })
        );
        assert_eq!(put.method, "PUT");
        assert_eq!(put.target, "/r2/room-123456/song.mp3?X-Amz-Signature=abc");
        // The signed type exactly, and a length: R2 refuses a chunked PUT.
        assert_eq!(put.header("content-type"), Some("audio/mpeg"));
        assert_eq!(put.header("content-length"), Some("150000"));
        assert_eq!(put.header("transfer-encoding"), None);
        assert!(put.body == data, "the PUT body is the file");
        assert_eq!(
            (complete.method.as_str(), complete.target.as_str()),
            ("POST", "/upload/complete")
        );
        assert_eq!(
            complete.json(),
            json!({
                "roomId": "123456",
                "originalName": "Song.MP3",
                "publicUrl": "https://pub.example/room-123456/song.mp3",
            })
        );
        result.unwrap();
    }

    #[tokio::test]
    async fn upload_errors_carry_the_server_text() {
        let dir = TempDir::new("upload-errors");
        let song = write_file(&dir, "song.mp3", &body(1_000));

        let no_storage =
            TestServer::start(|_| Reply::text(500, "R2 configuration not complete")).await;
        let err = upload_audio(&client(), &no_storage.url("/"), "123456", &song)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "R2 configuration not complete");
        assert_eq!(no_storage.requests().len(), 1);

        let text = write_file(&dir, "notes.txt", b"hello");
        let err = upload_audio(&client(), &no_storage.url("/"), "123456", &text)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "Please select an audio file");
        assert_eq!(
            no_storage.requests().len(),
            1,
            "no request for a non-audio file"
        );

        let silent = TestServer::start(|_| Reply::status(502)).await;
        let err = upload_audio(&client(), &silent.url("/"), "123456", &song)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "Upload failed: 502 Bad Gateway");

        let refused_put = TestServer::start(upload_server_reply(|| Reply::status(403))).await;
        let err = upload_audio(&client(), &refused_put.url("/"), "123456", &song)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "Upload failed: 403 Forbidden");
        assert_eq!(
            refused_put.requests().len(),
            2,
            "no complete after a failed PUT"
        );
    }
}
