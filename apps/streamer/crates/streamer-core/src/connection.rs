//! WebSocket connection shell: connect, reconnect with backoff, NTP probe pairs.

use crate::clock::epoch_ms;
use crate::ntp::PROBE_GAP_MS;
use crate::protocol::{ClientMessage, ServerMessage};
use crate::room;
use futures_util::{SinkExt, StreamExt};
use std::sync::Once;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

#[derive(Debug, PartialEq)]
pub enum ConnError {
    InvalidUrl(String),
    UnsupportedScheme(String),
}

impl std::fmt::Display for ConnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnError::InvalidUrl(e) => write!(f, "invalid server URL: {e}"),
            ConnError::UnsupportedScheme(s) => write!(f, "unsupported URL scheme: {s}"),
        }
    }
}

impl std::error::Error for ConnError {}

#[derive(Debug)]
pub enum Outgoing {
    Json(ClientMessage),
    Binary(Vec<u8>),
    /// A room action of the window, sent as it is.
    Action(serde_json::Value),
    ProbePair {
        group_id: u64,
        client_rtt: Option<f64>,
        client_nudge_ms: f64,
    },
}

#[derive(Debug)]
pub enum ConnEvent {
    Connected,
    Disconnected,
    /// A connect attempt failed; the reason, e.g. "IO error: Connection refused (os error
    /// 111)". The next attempt follows after the backoff.
    ConnectFailed(String),
    /// A server message for the window ([`room::forwarded`]), raw. It is followed by the
    /// parsed [`ConnEvent::Message`] of the same text when the core can parse it.
    Room(serde_json::Value),
    Message(ServerMessage),
    NtpResponse {
        t0: f64,
        t1: f64,
        t2: f64,
        t3: f64,
        group_id: u64,
        index: u8,
    },
}

/// The client kind this process announces to the server when it joins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Streamer,
    App,
}

impl JoinKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Streamer => "streamer",
            Self::App => "app",
        }
    }
}

pub fn build_ws_url(
    server: &str,
    room: &str,
    username: &str,
    client_id: &str,
    kind: JoinKind,
) -> Result<String, ConnError> {
    let mut url = url::Url::parse(server).map_err(|e| ConnError::InvalidUrl(e.to_string()))?;
    match url.scheme() {
        "ws" | "wss" => {}
        other => return Err(ConnError::UnsupportedScheme(other.to_string())),
    }
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| !matches!(k.as_ref(), "roomId" | "username" | "clientId" | "kind"))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    url.set_query(None);
    url.query_pairs_mut()
        .extend_pairs(kept)
        .append_pair("roomId", room)
        .append_pair("username", username)
        .append_pair("clientId", client_id)
        .append_pair("kind", kind.as_str());
    Ok(url.into())
}

const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// No message from the server for this long ends the session. While connected the engine
/// sends a probe pair at least every 2.5 s and the server answers each, so 10 s of silence
/// means a dead socket (e.g. after a network change) that would stay open for minutes.
const IDLE_TIMEOUT: Duration = Duration::from_secs(10);
/// A connect (TCP, TLS and WebSocket handshake) that takes longer is a failed attempt.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(1);
const BACKOFF_SECS: [u64; 5] = [1, 2, 4, 8, 10];

pub(crate) fn install_crypto_provider() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Resolves when shutdown is requested (or the sender is dropped).
async fn wait_shutdown(rx: &mut watch::Receiver<bool>) {
    loop {
        if *rx.borrow() {
            return;
        }
        if rx.changed().await.is_err() {
            return;
        }
    }
}

enum SessionEnd {
    Shutdown,
    Lost,
    Stop,
}

struct PendingProbe {
    deadline: Instant,
    group_id: u64,
    client_rtt: Option<f64>,
    client_nudge_ms: f64,
}

fn probe_message(
    group_id: u64,
    client_rtt: Option<f64>,
    client_nudge_ms: f64,
    index: u8,
) -> Message {
    let msg = ClientMessage::NtpRequest {
        t0: epoch_ms(),
        client_rtt,
        client_compensation_ms: None,
        client_nudge_ms: Some(client_nudge_ms),
        probe_group_id: group_id,
        probe_group_index: index,
    };
    json_message(&msg)
}

fn json_message(msg: &ClientMessage) -> Message {
    // Serialization of these plain types cannot fail.
    Message::text(serde_json::to_string(msg).unwrap_or_default())
}

/// Sends the events of one text message from the server, received at `t3`: the raw message
/// for the window first (when [`room::forwarded`]), then the parsed one for the engine. A
/// message that does not parse is logged and dropped. False when the receiver is gone.
fn send_text_events(text: &str, t3: f64, ev_tx: &mpsc::UnboundedSender<ConnEvent>) -> bool {
    let value: serde_json::Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(e) => {
            eprintln!("streamer: cannot parse server message: {e}");
            return true;
        }
    };
    if room::forwarded(&value) && ev_tx.send(ConnEvent::Room(value.clone())).is_err() {
        return false;
    }
    let event = match serde_json::from_value::<ServerMessage>(value) {
        Ok(ServerMessage::NtpResponse {
            t0,
            t1,
            t2,
            probe_group_id,
            probe_group_index,
        }) => ConnEvent::NtpResponse {
            t0,
            t1,
            t2,
            t3,
            group_id: probe_group_id,
            index: probe_group_index,
        },
        Ok(message) => ConnEvent::Message(message),
        Err(e) => {
            eprintln!("streamer: cannot parse server message: {e}");
            return true;
        }
    };
    ev_tx.send(event).is_ok()
}

async fn session(
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    out_rx: &mut mpsc::Receiver<Outgoing>,
    ev_tx: &mpsc::UnboundedSender<ConnEvent>,
    shutdown: &mut watch::Receiver<bool>,
) -> SessionEnd {
    let (mut write, mut read) = ws.split();
    let mut pending: Option<PendingProbe> = None;
    let idle = tokio::time::sleep(IDLE_TIMEOUT);
    tokio::pin!(idle);
    loop {
        let probe_wait = async {
            match &pending {
                Some(p) => tokio::time::sleep_until(p.deadline).await,
                None => std::future::pending().await,
            }
        };
        let mut to_send: Vec<Message> = Vec::new();
        tokio::select! {
            _ = wait_shutdown(shutdown) => {
                let _ = tokio::time::timeout(CLOSE_TIMEOUT, write.send(Message::Close(None))).await;
                return SessionEnd::Shutdown;
            }
            _ = &mut idle => {
                eprintln!(
                    "streamer: no message from the server for {} s, reconnecting",
                    IDLE_TIMEOUT.as_secs()
                );
                return SessionEnd::Lost;
            }
            _ = probe_wait => {
                if let Some(p) = pending.take() {
                    to_send.push(probe_message(p.group_id, p.client_rtt, p.client_nudge_ms, 1));
                }
            }
            out = out_rx.recv() => match out {
                None => return SessionEnd::Stop,
                Some(Outgoing::Json(m)) => to_send.push(json_message(&m)),
                Some(Outgoing::Binary(b)) => to_send.push(Message::binary(b)),
                Some(Outgoing::Action(action)) => to_send.push(Message::text(action.to_string())),
                Some(Outgoing::ProbePair { group_id, client_rtt, client_nudge_ms }) => {
                    if let Some(p) = pending.take() {
                        to_send.push(probe_message(p.group_id, p.client_rtt, p.client_nudge_ms, 1));
                    }
                    to_send.push(probe_message(group_id, client_rtt, client_nudge_ms, 0));
                    pending = Some(PendingProbe {
                        deadline: Instant::now() + Duration::from_millis(PROBE_GAP_MS as u64),
                        group_id,
                        client_rtt,
                        client_nudge_ms,
                    });
                }
            },
            incoming = read.next() => {
                if matches!(incoming, Some(Ok(_))) {
                    idle.as_mut().reset(Instant::now() + IDLE_TIMEOUT);
                }
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        if !send_text_events(text.as_str(), epoch_ms(), ev_tx) {
                            return SessionEnd::Stop;
                        }
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return SessionEnd::Lost,
                    Some(Ok(_)) => {}
                }
            }
        }
        for m in to_send {
            tokio::select! {
                _ = wait_shutdown(shutdown) => {
                    let _ = tokio::time::timeout(CLOSE_TIMEOUT, write.send(Message::Close(None))).await;
                    return SessionEnd::Shutdown;
                }
                r = tokio::time::timeout(WRITE_TIMEOUT, write.send(m)) => {
                    if !matches!(r, Ok(Ok(()))) {
                        return SessionEnd::Lost;
                    }
                }
            }
        }
    }
}

/// Connects, serves the socket and reconnects forever until `shutdown` is `true`.
pub async fn run_connection(
    url: String,
    mut out_rx: mpsc::Receiver<Outgoing>,
    ev_tx: mpsc::UnboundedSender<ConnEvent>,
    mut shutdown: watch::Receiver<bool>,
) {
    install_crypto_provider();
    let mut attempt = 0usize;
    loop {
        if *shutdown.borrow() {
            return;
        }
        let connect = tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio_tungstenite::connect_async_with_config(url.as_str(), None, true),
        );
        tokio::pin!(connect);
        let result = loop {
            tokio::select! {
                _ = wait_shutdown(&mut shutdown) => return,
                r = &mut connect => break r,
                out = out_rx.recv() => if out.is_none() { return },
            }
        };
        match result {
            Ok(Ok((ws, _))) => {
                attempt = 0;
                if ev_tx.send(ConnEvent::Connected).is_err() {
                    return;
                }
                match session(ws, &mut out_rx, &ev_tx, &mut shutdown).await {
                    SessionEnd::Shutdown | SessionEnd::Stop => return,
                    SessionEnd::Lost => {
                        if ev_tx.send(ConnEvent::Disconnected).is_err() {
                            return;
                        }
                    }
                }
            }
            Ok(Err(e)) => {
                eprintln!("streamer: connect failed: {e}");
                if ev_tx.send(ConnEvent::ConnectFailed(e.to_string())).is_err() {
                    return;
                }
            }
            Err(_) => {
                let error = format!("timed out after {} s", CONNECT_TIMEOUT.as_secs());
                eprintln!("streamer: connect {error}");
                if ev_tx.send(ConnEvent::ConnectFailed(error)).is_err() {
                    return;
                }
            }
        }
        let delay = Duration::from_secs(BACKOFF_SECS[attempt.min(BACKOFF_SECS.len() - 1)]);
        attempt += 1;
        let sleep = tokio::time::sleep(delay);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = wait_shutdown(&mut shutdown) => return,
                _ = &mut sleep => break,
                out = out_rx.recv() => if out.is_none() { return },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::epoch_ms;
    use crate::protocol::RoomEvent;
    use futures_util::{SinkExt, StreamExt};
    use std::time::{Duration, Instant};
    use tokio::net::TcpListener;
    use tokio::time::timeout;
    use tokio_tungstenite::tungstenite::Message;

    type Ws = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

    struct Harness {
        listener: TcpListener,
        url: String,
        out_tx: mpsc::Sender<Outgoing>,
        ev_rx: mpsc::UnboundedReceiver<ConnEvent>,
        shutdown_tx: watch::Sender<bool>,
        task: tokio::task::JoinHandle<()>,
    }

    async fn harness() -> Harness {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/ws?roomId=123456", listener.local_addr().unwrap());
        let (out_tx, out_rx) = mpsc::channel(64);
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(run_connection(url.clone(), out_rx, ev_tx, shutdown_rx));
        Harness {
            listener,
            url,
            out_tx,
            ev_rx,
            shutdown_tx,
            task,
        }
    }

    async fn accept(l: &TcpListener) -> Ws {
        let (s, _) = timeout(Duration::from_secs(4), l.accept())
            .await
            .unwrap()
            .unwrap();
        tokio_tungstenite::accept_async(s).await.unwrap()
    }

    async fn next_ev(h: &mut Harness) -> ConnEvent {
        timeout(Duration::from_secs(4), h.ev_rx.recv())
            .await
            .unwrap()
            .unwrap()
    }

    async fn next_msg(ws: &mut Ws) -> Message {
        timeout(Duration::from_secs(4), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    }

    #[test]
    fn url_has_params_and_kind_streamer() {
        let u = build_ws_url(
            "wss://example.com/ws?x=1",
            "123456",
            "a b&c",
            "id-1",
            JoinKind::Streamer,
        )
        .unwrap();
        let parsed = url::Url::parse(&u).unwrap();
        let pairs: Vec<(String, String)> = parsed
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("x".to_string(), "1".to_string()),
                ("roomId".to_string(), "123456".to_string()),
                ("username".to_string(), "a b&c".to_string()),
                ("clientId".to_string(), "id-1".to_string()),
                ("kind".to_string(), "streamer".to_string()),
            ]
        );
        assert!(!u.contains("a b&c"));
        assert!(build_ws_url("http://example.com/ws", "1", "u", "c", JoinKind::Streamer).is_err());
        assert!(build_ws_url("not a url", "1", "u", "c", JoinKind::Streamer).is_err());
    }

    #[test]
    fn url_replaces_reserved_keys_from_base() {
        let u = build_ws_url(
            "wss://h/ws?roomId=1&x=1&kind=old&username=z&clientId=q",
            "2",
            "u",
            "c",
            JoinKind::Streamer,
        )
        .unwrap();
        let pairs: Vec<(String, String)> = url::Url::parse(&u)
            .unwrap()
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        let keys: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["x", "roomId", "username", "clientId", "kind"]);
        assert_eq!(pairs[1].1, "2");
        assert_eq!(pairs[4].1, "streamer");
    }

    #[test]
    fn url_has_exactly_one_kind_pair_for_each_join_kind() {
        for (kind, want) in [(JoinKind::App, "app"), (JoinKind::Streamer, "streamer")] {
            let u = build_ws_url("ws://h/ws?kind=web", "123456", "Ann", "id", kind).unwrap();
            let kinds: Vec<String> = url::Url::parse(&u)
                .unwrap()
                .query_pairs()
                .filter(|(k, _)| k == "kind")
                .map(|(_, v)| v.into_owned())
                .collect();
            assert_eq!(kinds, [want]);
        }
    }

    #[tokio::test]
    async fn shutdown_returns_while_the_peer_stops_reading() {
        let mut h = harness().await;
        let _ws = accept(&h.listener).await; // never read
        assert!(matches!(next_ev(&mut h).await, ConnEvent::Connected));
        let tx = h.out_tx.clone();
        tokio::spawn(
            async move { while tx.send(Outgoing::Binary(vec![0u8; 65536])).await.is_ok() {} },
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
        h.shutdown_tx.send(true).unwrap();
        timeout(Duration::from_secs(2), h.task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn silent_server_ends_the_session_after_10_s() {
        // A half-open socket (Wi-Fi drop, IP change) gives no error for minutes: the status
        // stays "synced" and a start hangs in Starting.
        let mut h = harness().await;
        let _ws = accept(&h.listener).await; // accepts, then never sends
        assert!(matches!(next_ev(&mut h).await, ConnEvent::Connected));
        let begin = Instant::now();
        let ev = timeout(Duration::from_secs(13), h.ev_rx.recv())
            .await
            .expect("the session ends")
            .unwrap();
        assert!(matches!(ev, ConnEvent::Disconnected), "{ev:?}");
        let silent = begin.elapsed();
        assert!(silent >= Duration::from_millis(9_500), "after {silent:?}");
        h.shutdown_tx.send(true).unwrap();
    }

    #[tokio::test]
    async fn unanswered_handshake_is_retried() {
        let h = harness().await;
        // The TCP connection is accepted, the WebSocket handshake never answered.
        let (first, _) = timeout(Duration::from_secs(4), h.listener.accept())
            .await
            .unwrap()
            .unwrap();
        let begin = Instant::now();
        let (second, _) = timeout(Duration::from_secs(14), h.listener.accept())
            .await
            .expect("a new attempt after the connect timeout and the backoff")
            .unwrap();
        let waited = begin.elapsed();
        assert!(waited >= Duration::from_millis(9_500), "after {waited:?}");
        drop((first, second));
        h.shutdown_tx.send(true).unwrap();
    }

    #[tokio::test]
    async fn sends_json_and_binary() {
        let mut h = harness().await;
        let mut ws = accept(&h.listener).await;
        assert!(matches!(next_ev(&mut h).await, ConnEvent::Connected));
        h.out_tx
            .send(Outgoing::Json(ClientMessage::LivenessPong))
            .await
            .unwrap();
        h.out_tx
            .send(Outgoing::Binary(vec![1, 2, 3]))
            .await
            .unwrap();
        match next_msg(&mut ws).await {
            Message::Text(t) => assert_eq!(t.as_str(), r#"{"type":"LIVENESS_PONG"}"#),
            m => panic!("unexpected {m:?}"),
        }
        match next_msg(&mut ws).await {
            Message::Binary(b) => assert_eq!(b.as_ref(), &[1, 2, 3]),
            m => panic!("unexpected {m:?}"),
        }
        h.shutdown_tx.send(true).unwrap();
        timeout(Duration::from_secs(4), h.task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn probe_pair_is_two_requests_about_25_ms_apart() {
        let mut h = harness().await;
        let mut ws = accept(&h.listener).await;
        assert!(matches!(next_ev(&mut h).await, ConnEvent::Connected));
        h.out_tx
            .send(Outgoing::ProbePair {
                group_id: 7,
                client_rtt: Some(12.5),
                client_nudge_ms: 7.5,
            })
            .await
            .unwrap();
        let mut got = Vec::new();
        for _ in 0..2 {
            let m = next_msg(&mut ws).await;
            let at = Instant::now();
            let Message::Text(t) = m else {
                panic!("not text")
            };
            let v: serde_json::Value = serde_json::from_str(t.as_str()).unwrap();
            got.push((at, v));
        }
        assert_eq!(got[0].1["type"], "NTP_REQUEST");
        assert_eq!(got[0].1["probeGroupIndex"], 0);
        assert_eq!(got[1].1["probeGroupIndex"], 1);
        assert_eq!(got[0].1["probeGroupId"], 7);
        assert_eq!(got[1].1["probeGroupId"], 7);
        assert_eq!(got[0].1["clientRTT"], 12.5);
        assert_eq!(got[1].1["clientRTT"], 12.5);
        assert_eq!(got[0].1["clientNudgeMs"], 7.5);
        assert_eq!(got[1].1["clientNudgeMs"], 7.5);
        assert!(got[0].1.get("clientCompensationMs").is_none());
        let gap = got[1].0.duration_since(got[0].0).as_secs_f64() * 1000.0;
        assert!((20.0..40.0).contains(&gap), "gap {gap} ms");
        h.shutdown_tx.send(true).unwrap();
    }

    #[tokio::test]
    async fn reconnects_after_server_close() {
        let mut h = harness().await;
        let mut ws = accept(&h.listener).await;
        assert!(matches!(next_ev(&mut h).await, ConnEvent::Connected));
        ws.close(None).await.unwrap();
        drop(ws);
        assert!(matches!(next_ev(&mut h).await, ConnEvent::Disconnected));
        let _ws2 = accept(&h.listener).await;
        assert!(matches!(next_ev(&mut h).await, ConnEvent::Connected));
        h.shutdown_tx.send(true).unwrap();
        let _ = &h.url;
    }

    #[tokio::test]
    async fn ntp_response_gets_receive_timestamp() {
        let mut h = harness().await;
        let mut ws = accept(&h.listener).await;
        assert!(matches!(next_ev(&mut h).await, ConnEvent::Connected));
        let before = epoch_ms();
        ws.send(Message::text(
            r#"{"type":"NTP_RESPONSE","t0":1.0,"t1":2.0,"t2":3.0,"probeGroupId":9,"probeGroupIndex":1}"#,
        ))
        .await
        .unwrap();
        match next_ev(&mut h).await {
            ConnEvent::NtpResponse {
                t0,
                t1,
                t2,
                t3,
                group_id,
                index,
            } => {
                assert_eq!((t0, t1, t2, group_id, index), (1.0, 2.0, 3.0, 9, 1));
                assert!(t3 >= before && t3 <= epoch_ms());
            }
            e => panic!("unexpected {e:?}"),
        }
        ws.send(Message::text(r#"{"type":"LIVENESS_PING"}"#))
            .await
            .unwrap();
        assert!(matches!(
            next_ev(&mut h).await,
            ConnEvent::Message(ServerMessage::LivenessPing)
        ));
        h.shutdown_tx.send(true).unwrap();
    }

    #[tokio::test]
    async fn room_messages_reach_the_feed_and_the_engine() {
        // The window gets the raw room message; the engine still needs its parsed copy (the
        // client list, the stream state). Clock sync and liveness stay in the core.
        let mut h = harness().await;
        let mut ws = accept(&h.listener).await;
        assert!(matches!(next_ev(&mut h).await, ConnEvent::Connected));
        for text in [
            r#"{"type":"ROOM_EVENT","event":{"type":"CLIENT_CHANGE","clients":[{"username":"a","clientId":"1","isAdmin":true,"joinedAt":1}]}}"#,
            r#"{"type":"NTP_RESPONSE","t0":1.0,"t1":2.0,"t2":3.0,"probeGroupId":9,"probeGroupIndex":1}"#,
            r#"{"type":"LIVENESS_PING"}"#,
        ] {
            ws.send(Message::text(text)).await.unwrap();
        }
        match next_ev(&mut h).await {
            ConnEvent::Room(v) => {
                assert_eq!(v["type"], "ROOM_EVENT");
                assert_eq!(v["event"]["clients"][0]["clientId"], "1");
            }
            e => panic!("unexpected {e:?}"),
        }
        match next_ev(&mut h).await {
            ConnEvent::Message(ServerMessage::RoomEvent {
                event: RoomEvent::ClientChange { clients },
            }) => assert_eq!(clients[0].client_id, "1"),
            e => panic!("unexpected {e:?}"),
        }
        assert!(matches!(
            next_ev(&mut h).await,
            ConnEvent::NtpResponse { group_id: 9, .. }
        ));
        assert!(matches!(
            next_ev(&mut h).await,
            ConnEvent::Message(ServerMessage::LivenessPing)
        ));
        h.shutdown_tx.send(true).unwrap();
    }

    #[tokio::test]
    async fn actions_go_out_as_json() {
        let mut h = harness().await;
        let mut ws = accept(&h.listener).await;
        assert!(matches!(next_ev(&mut h).await, ConnEvent::Connected));
        let action = serde_json::json!({ "type": "SEND_CHAT_MESSAGE", "text": "hi" });
        h.out_tx
            .send(Outgoing::Action(action.clone()))
            .await
            .unwrap();
        match next_msg(&mut ws).await {
            Message::Text(t) => {
                let sent: serde_json::Value = serde_json::from_str(t.as_str()).unwrap();
                assert_eq!(sent, action);
            }
            m => panic!("unexpected {m:?}"),
        }
        h.shutdown_tx.send(true).unwrap();
    }
}
