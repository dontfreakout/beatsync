//! A local plain-HTTP server for tests: it records each request (head and body) and
//! answers with the reply its handler picks.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use url::Url;

/// One request as it came over the wire. Header names are lowercase.
#[derive(Debug, Clone)]
pub(crate) struct Request {
    pub(crate) method: String,
    /// The request target: path and query.
    pub(crate) target: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl Request {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub(crate) fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("a JSON body")
    }
}

pub(crate) struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
    content_length: bool,
    /// Sends only this many body bytes, then nothing more until the client goes away.
    stall_after: Option<usize>,
}

impl Reply {
    pub(crate) fn status(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
            content_length: true,
            stall_after: None,
        }
    }

    pub(crate) fn bytes(body: Vec<u8>) -> Self {
        Self {
            body,
            ..Self::status(200)
        }
    }

    pub(crate) fn json(value: serde_json::Value) -> Self {
        Self {
            headers: vec![("Content-Type", "application/json".to_string())],
            ..Self::bytes(value.to_string().into_bytes())
        }
    }

    /// A text/plain reply, like the server's `errorResponse`.
    pub(crate) fn text(status: u16, text: &str) -> Self {
        Self {
            headers: vec![("Content-Type", "text/plain;charset=utf-8".to_string())],
            body: text.as_bytes().to_vec(),
            ..Self::status(status)
        }
    }

    /// No Content-Length header: the body ends when the connection closes.
    pub(crate) fn without_content_length(self) -> Self {
        Self {
            content_length: false,
            ..self
        }
    }

    /// Announces the whole body but sends only its first `bytes`, then stalls: a download that
    /// is cut off mid-body.
    pub(crate) fn stalled_after(self, bytes: usize) -> Self {
        Self {
            stall_after: Some(bytes.min(self.body.len())),
            ..self
        }
    }
}

/// Serves until dropped. Every reply has `Connection: close`, so the shared client never
/// keeps a connection that outlives the runtime of one test.
pub(crate) struct TestServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Request>>>,
    task: tokio::task::JoinHandle<()>,
}

impl TestServer {
    pub(crate) async fn start<F>(handler: F) -> Self
    where
        F: Fn(&Request) -> Reply + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let handler = Arc::new(handler);
        let recorded = requests.clone();
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let handler = handler.clone();
                let recorded = recorded.clone();
                tokio::spawn(async move {
                    let Some(request) = read_request(&mut stream).await else {
                        return;
                    };
                    let reply = handler(&request);
                    // Recorded before the reply goes out: once the client has the answer,
                    // the request is in the list.
                    recorded.lock().unwrap().push(request);
                    let _ = write_reply(&mut stream, reply).await;
                });
            }
        });
        Self {
            addr,
            requests,
            task,
        }
    }

    /// The URL of `path` (with an optional query) on this server.
    pub(crate) fn url(&self, path: &str) -> Url {
        Url::parse(&format!("http://{}{path}", self.addr)).unwrap()
    }

    pub(crate) fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Reads the head and then the body by Content-Length (a chunked body is not decoded: the
/// tests that send one check that no request is chunked).
async fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = std::str::from_utf8(&buf[..head_end]).ok()?;
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_string();
    let target = first.next()?.to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let length: usize = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = buf[head_end..].to_vec();
    while body.len() < length {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Some(Request {
        method,
        target,
        headers,
        body,
    })
}

async fn write_reply(stream: &mut TcpStream, reply: Reply) -> std::io::Result<()> {
    let reason = reqwest::StatusCode::from_u16(reply.status)
        .ok()
        .and_then(|s| s.canonical_reason())
        .unwrap_or("");
    let mut head = format!(
        "HTTP/1.1 {} {reason}\r\nConnection: close\r\n",
        reply.status
    );
    for (name, value) in &reply.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if reply.content_length {
        head.push_str(&format!("Content-Length: {}\r\n", reply.body.len()));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    if let Some(sent) = reply.stall_after {
        stream.write_all(&reply.body[..sent]).await?;
        return std::future::pending().await;
    }
    stream.write_all(&reply.body).await?;
    stream.shutdown().await
}
