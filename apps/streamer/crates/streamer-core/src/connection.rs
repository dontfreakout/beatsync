//! WebSocket connection shell: connect, reconnect with backoff, NTP probe pairs.

use crate::clock::epoch_ms;
use crate::ntp::PROBE_GAP_MS;
use crate::protocol::{ClientMessage, ServerMessage};
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
    ProbePair {
        group_id: u64,
        client_rtt: Option<f64>,
    },
}

#[derive(Debug)]
pub enum ConnEvent {
    Connected,
    Disconnected,
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

pub fn build_ws_url(
    server: &str,
    room: &str,
    username: &str,
    client_id: &str,
) -> Result<String, ConnError> {
    let mut url = url::Url::parse(server).map_err(|e| ConnError::InvalidUrl(e.to_string()))?;
    match url.scheme() {
        "ws" | "wss" => {}
        other => return Err(ConnError::UnsupportedScheme(other.to_string())),
    }
    url.query_pairs_mut()
        .append_pair("roomId", room)
        .append_pair("username", username)
        .append_pair("clientId", client_id)
        .append_pair("kind", "streamer");
    Ok(url.into())
}

const BACKOFF_SECS: [u64; 5] = [1, 2, 4, 8, 10];

fn install_crypto_provider() {
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
}

fn probe_message(group_id: u64, client_rtt: Option<f64>, index: u8) -> Message {
    let msg = ClientMessage::NtpRequest {
        t0: epoch_ms(),
        client_rtt,
        client_compensation_ms: None,
        client_nudge_ms: None,
        probe_group_id: group_id,
        probe_group_index: index,
    };
    json_message(&msg)
}

fn json_message(msg: &ClientMessage) -> Message {
    // Serialization of these plain types cannot fail.
    Message::text(serde_json::to_string(msg).unwrap_or_default())
}

async fn session(
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    out_rx: &mut mpsc::Receiver<Outgoing>,
    ev_tx: &mpsc::UnboundedSender<ConnEvent>,
    shutdown: &mut watch::Receiver<bool>,
) -> SessionEnd {
    let (mut write, mut read) = ws.split();
    let mut pending: Option<PendingProbe> = None;
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
                let _ = write.send(Message::Close(None)).await;
                return SessionEnd::Shutdown;
            }
            _ = probe_wait => {
                if let Some(p) = pending.take() {
                    to_send.push(probe_message(p.group_id, p.client_rtt, 1));
                }
            }
            out = out_rx.recv() => match out {
                None => return SessionEnd::Stop,
                Some(Outgoing::Json(m)) => to_send.push(json_message(&m)),
                Some(Outgoing::Binary(b)) => to_send.push(Message::binary(b)),
                Some(Outgoing::ProbePair { group_id, client_rtt }) => {
                    if let Some(p) = pending.take() {
                        to_send.push(probe_message(p.group_id, p.client_rtt, 1));
                    }
                    to_send.push(probe_message(group_id, client_rtt, 0));
                    pending = Some(PendingProbe {
                        deadline: Instant::now() + Duration::from_millis(PROBE_GAP_MS as u64),
                        group_id,
                        client_rtt,
                    });
                }
            },
            incoming = read.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    let t3 = epoch_ms();
                    match serde_json::from_str::<ServerMessage>(text.as_str()) {
                        Ok(ServerMessage::NtpResponse { t0, t1, t2, probe_group_id, probe_group_index }) => {
                            let ev = ConnEvent::NtpResponse {
                                t0, t1, t2, t3,
                                group_id: probe_group_id,
                                index: probe_group_index,
                            };
                            if ev_tx.send(ev).is_err() {
                                return SessionEnd::Stop;
                            }
                        }
                        Ok(m) => {
                            if ev_tx.send(ConnEvent::Message(m)).is_err() {
                                return SessionEnd::Stop;
                            }
                        }
                        Err(e) => eprintln!("streamer: cannot parse server message: {e}"),
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return SessionEnd::Lost,
                Some(Ok(_)) => {}
            },
        }
        for m in to_send {
            if write.send(m).await.is_err() {
                return SessionEnd::Lost;
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
        let connect = tokio_tungstenite::connect_async_with_config(url.as_str(), None, true);
        tokio::pin!(connect);
        let result = loop {
            tokio::select! {
                _ = wait_shutdown(&mut shutdown) => return,
                r = &mut connect => break r,
                out = out_rx.recv() => if out.is_none() { return },
            }
        };
        match result {
            Ok((ws, _)) => {
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
            Err(e) => eprintln!("streamer: connect failed: {e}"),
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
        let u = build_ws_url("wss://example.com/ws?x=1", "123456", "a b&c", "id-1").unwrap();
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
        assert!(build_ws_url("http://example.com/ws", "1", "u", "c").is_err());
        assert!(build_ws_url("not a url", "1", "u", "c").is_err());
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
}
