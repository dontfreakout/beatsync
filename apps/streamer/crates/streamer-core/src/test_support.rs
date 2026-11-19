//! Helpers shared by the runtime and track loader tests: a recording audio backend and a
//! local room server.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::audio::{AppInfo, AudioBackend, AudioError, AudioIo, OutputIo, PlayoutShared};
use crate::config::Settings;
use crate::connection::JoinKind;
use crate::runtime::StreamerHandle;
use crate::test_util::TempDir;
use serde_json::{Value, json};

/// Records the backend calls; `start` and `open_output` succeed and keep the audio ends like
/// a real backend, until `stop` and `close_output`.
#[derive(Default)]
pub(crate) struct FakeBackend {
    calls: std::sync::Mutex<Vec<&'static str>>,
    io: std::sync::Mutex<Option<AudioIo>>,
    output: std::sync::Mutex<Option<OutputIo>>,
    /// Whether the track slot's producer was gone when the output closed.
    track_producer_gone_at_close: std::sync::Mutex<Option<bool>>,
    /// When the output closed, with its gain and gain ramp then.
    output_closed: std::sync::Mutex<Option<ClosedOutput>>,
}

/// When an output the backend kept closed, with the gain it was set to then.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ClosedOutput {
    pub(crate) at: Instant,
    pub(crate) gain: f32,
    pub(crate) gain_ramp_s: f32,
}

impl FakeBackend {
    pub(crate) fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().unwrap().clone()
    }

    /// The ends of the running audio session, for a test that plays the capture.
    pub(crate) fn take_io(&self) -> Option<AudioIo> {
        self.io.lock().unwrap().take()
    }

    /// The ends of the open output, for a test that plays the output.
    pub(crate) fn take_output(&self) -> Option<OutputIo> {
        self.output.lock().unwrap().take()
    }

    /// The shared values of the open output and the number of blocks queued in its track
    /// slot, while the backend keeps the output.
    pub(crate) fn output_track(&self) -> Option<(Arc<PlayoutShared>, usize)> {
        let output = self.output.lock().unwrap();
        output
            .as_ref()
            .map(|io| (io.shared.clone(), io.track_rx.slots()))
    }

    /// Whether the open output low-passes its mix.
    pub(crate) fn output_lowpass(&self) -> Option<bool> {
        self.output.lock().unwrap().as_ref().map(|io| io.lowpass)
    }

    /// Whether the track slot's producer was gone (its feeder ended) when the output closed;
    /// `None` until an output the backend kept closes.
    pub(crate) fn track_producer_gone_at_close(&self) -> Option<bool> {
        *self.track_producer_gone_at_close.lock().unwrap()
    }

    /// When the output the backend kept closed; `None` until then.
    pub(crate) fn output_closed(&self) -> Option<ClosedOutput> {
        *self.output_closed.lock().unwrap()
    }

    fn record(&self, call: &'static str) {
        self.calls.lock().unwrap().push(call);
    }
}

impl AudioBackend for FakeBackend {
    fn list_apps(&self) -> Result<Vec<AppInfo>, AudioError> {
        Ok(Vec::new())
    }

    fn start(&self, _apps: Vec<String>, io: AudioIo) -> Result<(), AudioError> {
        self.record("start");
        *self.io.lock().unwrap() = Some(io);
        Ok(())
    }

    fn update_apps(&self, _apps: Vec<String>) -> Result<(), AudioError> {
        self.record("update_apps");
        Ok(())
    }

    fn stop(&self) -> Result<(), AudioError> {
        self.record("stop");
        self.io.lock().unwrap().take();
        Ok(())
    }

    fn open_output(&self, io: OutputIo) -> Result<(), AudioError> {
        self.record("open_output");
        *self.output.lock().unwrap() = Some(io);
        Ok(())
    }

    fn close_output(&self) -> Result<(), AudioError> {
        self.record("close_output");
        if let Some(io) = self.output.lock().unwrap().take() {
            *self.track_producer_gone_at_close.lock().unwrap() = Some(io.track_rx.is_abandoned());
            let load = |atomic: &std::sync::atomic::AtomicU32| {
                f32::from_bits(atomic.load(Ordering::Relaxed))
            };
            *self.output_closed.lock().unwrap() = Some(ClosedOutput {
                at: Instant::now(),
                gain: load(&io.shared.gain),
                gain_ramp_s: load(&io.shared.gain_ramp_s),
            });
        }
        Ok(())
    }
}

pub(crate) const WAIT: Duration = Duration::from_secs(4);

pub(crate) fn settings(server_url: String) -> Settings {
    Settings {
        server_url,
        room_id: "123456".to_string(),
        username: "alice".to_string(),
        client_id: "me".to_string(),
        ..Settings::default()
    }
}

/// A local WebSocket server and the settings to join it.
pub(crate) async fn server() -> (tokio::net::TcpListener, Settings) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/ws", listener.local_addr().unwrap());
    (listener, settings(url))
}

pub(crate) type Ws = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

pub(crate) async fn accept(listener: &tokio::net::TcpListener) -> Ws {
    let (socket, _) = tokio::time::timeout(WAIT, listener.accept())
        .await
        .expect("the runtime connects")
        .unwrap();
    tokio_tungstenite::accept_async(socket).await.unwrap()
}

/// Answers every NTP_REQUEST on `ws` as a server whose clock is this machine's, until the
/// socket closes. The runtime then knows the server offset (about 0).
pub(crate) fn answer_ntp(mut ws: Ws) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(Ok(message)) = ws.next().await {
            let Some(response) = parse(&message).as_ref().and_then(ntp_response) else {
                continue;
            };
            if ws.send(Message::text(response.to_string())).await.is_err() {
                return;
            }
        }
    })
}

/// The JSON of a text message.
fn parse(message: &Message) -> Option<Value> {
    let Message::Text(text) = message else {
        return None;
    };
    serde_json::from_str(text.as_str()).ok()
}

/// The answer of a server whose clock is this machine's to an NTP_REQUEST; `None` for any
/// other message.
fn ntp_response(request: &Value) -> Option<Value> {
    if request["type"] != "NTP_REQUEST" {
        return None;
    }
    let t0 = request["t0"].clone();
    Some(json!({
        "type": "NTP_RESPONSE",
        "t0": t0,
        "t1": t0,
        "t2": t0,
        "probeGroupId": request["probeGroupId"],
        "probeGroupIndex": request["probeGroupIndex"],
    }))
}

/// The test's side of a joined room ([`serve_room`]).
pub(crate) struct Room {
    to_app: mpsc::UnboundedSender<Value>,
    from_app: mpsc::UnboundedReceiver<Value>,
}

impl Room {
    pub(crate) fn send(&self, message: Value) {
        self.to_app
            .send(message)
            .expect("the room serves the socket");
    }

    /// The next message of the app of type `kind`; others are skipped. Panics after [`WAIT`].
    pub(crate) async fn next_of(&mut self, kind: &str) -> Value {
        let next = async {
            loop {
                match self.from_app.recv().await {
                    Some(message) if message["type"] == kind => return message,
                    Some(_) => {}
                    None => panic!("the app closed the socket"),
                }
            }
        };
        tokio::time::timeout(WAIT, next)
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for a {kind} message"))
    }
}

/// Serves `ws` as a room: answers every NTP_REQUEST like [`answer_ntp`] (so the runtime
/// knows the server offset), sends the messages of [`Room::send`], and passes every other
/// message of the app to [`Room::next_of`].
pub(crate) fn serve_room(mut ws: Ws) -> Room {
    let (to_app, mut outgoing) = mpsc::unbounded_channel::<Value>();
    let (incoming, from_app) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                message = ws.next() => {
                    let Some(Ok(message)) = message else { return };
                    let Some(message) = parse(&message) else { continue };
                    match ntp_response(&message) {
                        Some(response) => {
                            if ws.send(Message::text(response.to_string())).await.is_err() {
                                return;
                            }
                        }
                        None => {
                            let _ = incoming.send(message);
                        }
                    }
                }
                Some(message) = outgoing.recv() => {
                    if ws.send(Message::text(message.to_string())).await.is_err() {
                        return;
                    }
                }
            }
        }
    });
    Room { to_app, from_app }
}

/// Polls `done` every few ms until it is true; panics after [`WAIT`].
pub(crate) async fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let poll = async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(WAIT, poll)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting until {what}"));
}

/// [`StreamerHandle::connect`] for the app with the track cache in `cache`: the loader
/// clears its cache dir, and no test may touch the user's.
pub(crate) async fn connect_app(
    settings: Settings,
    backend: Arc<FakeBackend>,
    room_feed: Option<mpsc::UnboundedSender<Value>>,
    cache: &TempDir,
) -> StreamerHandle {
    StreamerHandle::connect_in(settings, backend, JoinKind::App, room_feed, cache.0.clone()).await
}
