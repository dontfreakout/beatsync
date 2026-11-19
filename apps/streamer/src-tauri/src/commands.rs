//! Tauri commands and the app state behind them (spec §9.7).
//!
//! The state owns the settings, the audio backend and the connection ([`StreamerHandle`]).
//! Nothing connects at launch: [`join_room`] connects, [`leave_room`] stops the stream
//! (routing restored) and disconnects. Statuses, track states and room messages reach the
//! window through two [`LatestHub`]s and the [`RoomHub`], which outlive the connections. Each
//! join and each leave starts a new generation in all three, so nothing of an older connection
//! reaches the window.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::Serialize;
use serde_json::Value;
use tauri::ipc::Channel;
use tauri::{State, Webview};
use tokio::sync::{mpsc, watch};

use streamer_core::audio::{AppInfo, AudioBackend};
use streamer_core::config::{self, Settings};
use streamer_core::connection::{JoinKind, build_ws_url};
use streamer_core::engine::{Phase, Status};
use streamer_core::http;
use streamer_core::runtime::StreamerHandle;
use streamer_core::tracks::TrackStates;

use crate::room_hub::RoomHub;

pub type CommandResult<T> = Result<T, String>;

pub struct AppState {
    /// Kept for the app's lifetime: dropping the PipeWire backend joins its thread, which must
    /// not happen on a runtime worker. `Err` is the creation error that the UI shows in place
    /// of the app list (e.g. `PipeWire 1.0 and WirePlumber 0.5 or later required`).
    backend: Result<Arc<dyn AudioBackend>, String>,
    session: tokio::sync::Mutex<Session>,
    status_hub: Arc<LatestHub<Status>>,
    tracks_hub: Arc<LatestHub<TrackStates>>,
    room_hub: Arc<RoomHub<Channel<Value>>>,
    /// Set once the exit cleanup has started.
    pub exit_started: AtomicBool,
    /// Set once the exit cleanup has finished: the next exit request goes through.
    pub exit_done: AtomicBool,
}

struct Session {
    settings: Settings,
    /// The connection while a room is joined.
    handle: Option<StreamerHandle>,
    /// The exit cleanup has run: no new connection.
    closed: bool,
}

impl AppState {
    /// Loads the settings and creates the audio backend (blocks for up to ~2 s). Call it
    /// outside the async runtime.
    pub fn new() -> Self {
        let settings = config::load_or_create();
        let backend = new_backend();
        match &backend {
            Ok(_) => eprintln!("streamer-app: audio backend ready"),
            Err(e) => eprintln!("streamer-app: no audio backend: {e}"),
        }
        let status_hub = Arc::new(LatestHub::new("status", idle_status(settings.nudge_ms)));
        Self {
            backend,
            session: tokio::sync::Mutex::new(Session {
                settings,
                handle: None,
                closed: false,
            }),
            status_hub,
            tracks_hub: Arc::new(LatestHub::new("track states", TrackStates::new())),
            room_hub: Arc::new(RoomHub::new()),
            exit_started: AtomicBool::new(false),
            exit_done: AtomicBool::new(false),
        }
    }

    /// Stops the stream (the routing is restored when this returns) and closes the
    /// connection for good.
    pub async fn shutdown(&self) {
        let mut session = self.session.lock().await;
        session.closed = true;
        if let Some(handle) = session.handle.take() {
            eprintln!("streamer-app: stopping, restoring the routing");
            // `shutdown` stops first, then closes the connection.
            handle.shutdown().await;
            eprintln!("streamer-app: stopped");
        }
    }

    /// While no room is joined, the idle status shows the saved nudge.
    fn show_saved_nudge(&self, session: &Session) {
        if session.handle.is_none() {
            self.status_hub
                .next_generation(idle_status(session.settings.nudge_ms));
        }
    }

    /// Starts a new generation in every hub: the window gets the idle status (with the saved
    /// nudge), no track states and an empty room.
    fn next_generations(&self, settings: &Settings) -> Generations {
        Generations {
            status: self
                .status_hub
                .next_generation(idle_status(settings.nudge_ms)),
            tracks: self.tracks_hub.next_generation(TrackStates::new()),
            room: self.room_hub.next_generation(),
        }
    }
}

/// The hub generations of one connection.
struct Generations {
    status: u64,
    tracks: u64,
    room: u64,
}

#[cfg(target_os = "linux")]
fn new_backend() -> Result<Arc<dyn AudioBackend>, String> {
    use streamer_core::audio::pipewire::PipeWireBackend;
    PipeWireBackend::new()
        .map(|backend| Arc::new(backend) as Arc<dyn AudioBackend>)
        .map_err(|e| e.to_string())
}

#[cfg(not(target_os = "linux"))]
fn new_backend() -> Result<Arc<dyn AudioBackend>, String> {
    Err("Only Linux with PipeWire is supported".to_string())
}

fn save_error(e: std::io::Error) -> String {
    format!(
        "Cannot save the settings to {}: {e}",
        config::config_path().display()
    )
}

/// The checks of [`join_room`] that need no state. The server URL check is the one the
/// connection makes, so a join that passes connects to that server.
fn validate_join(room_id: &str, username: &str, server_url: &str) -> CommandResult<()> {
    if room_id.len() != 6 || !room_id.bytes().all(|b| b.is_ascii_digit()) {
        return Err("Room code must be 6 digits".to_string());
    }
    if username.trim().is_empty() {
        return Err("Enter a name".to_string());
    }
    build_ws_url(server_url, room_id, username, "", JoinKind::App)
        .map(drop)
        .map_err(|e| e.to_string())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    settings: Settings,
    joined: bool,
}

/// The saved settings and whether a room is joined (a reloaded window needs both).
#[tauri::command]
pub async fn get_session(state: State<'_, AppState>) -> CommandResult<SessionInfo> {
    let session = state.session.lock().await;
    Ok(SessionInfo {
        settings: session.settings.clone(),
        joined: session.handle.is_some(),
    })
}

/// Saves the settings (the `clientId` stays) and returns them normalized. Never reconnects:
/// the server, the room and the name apply at the next join. Nothing changes when the file
/// cannot be written.
#[tauri::command]
pub async fn save_settings(
    state: State<'_, AppState>,
    settings: Settings,
) -> CommandResult<Settings> {
    let mut session = state.session.lock().await;
    let mut next = Settings {
        client_id: session.settings.client_id.clone(),
        ..settings
    };
    next.normalize();
    config::save(&next).map_err(save_error)?;
    session.settings = next;
    state.show_saved_nudge(&session);
    Ok(session.settings.clone())
}

/// Saves the room and the name and connects to the room as kind `app`. Nothing connects when
/// a room is joined already or the room code, the name or the server URL is wrong. A
/// settings file that cannot be written does not stop the join (it is logged).
#[tauri::command]
pub async fn join_room(
    state: State<'_, AppState>,
    room_id: String,
    username: String,
) -> CommandResult<Settings> {
    let backend = state.backend.clone()?;
    let mut session = state.session.lock().await;
    if session.handle.is_some() {
        return Err("Leave the room first".to_string());
    }
    if session.closed {
        return Err("The app is closing".to_string());
    }
    let username = username.trim().to_string();
    validate_join(&room_id, &username, &session.settings.server_url)?;
    session.settings.room_id = room_id;
    session.settings.username = username;
    if let Err(e) = config::save(&session.settings) {
        eprintln!("streamer-app: {}", save_error(e));
    }

    let settings = session.settings.clone();
    let generations = state.next_generations(&settings);
    eprintln!(
        "streamer-app: joining {} room {} as {}",
        settings.server_url, settings.room_id, settings.username
    );
    let (feed, feed_rx) = mpsc::unbounded_channel();
    let handle = StreamerHandle::connect(settings, backend, JoinKind::App, Some(feed)).await;
    tauri::async_runtime::spawn(pump(
        handle.status(),
        state.status_hub.clone(),
        generations.status,
        status_logger(),
    ));
    tauri::async_runtime::spawn(pump(
        handle.tracks(),
        state.tracks_hub.clone(),
        generations.tracks,
        |_| {},
    ));
    tauri::async_runtime::spawn(room_pump(feed_rx, state.room_hub.clone(), generations.room));
    session.handle = Some(handle);
    Ok(session.settings.clone())
}

/// Stops the stream, restores the routing and disconnects. Returns after that, so the window
/// shows the join screen only once the routing is back. Ok when no room is joined.
#[tauri::command]
pub async fn leave_room(state: State<'_, AppState>) -> CommandResult<()> {
    let mut session = state.session.lock().await;
    if let Some(handle) = session.handle.take() {
        eprintln!("streamer-app: leaving the room, restoring the routing");
        // `shutdown` stops first, then closes the connection.
        handle.shutdown().await;
        eprintln!("streamer-app: left the room");
    }
    state.next_generations(&session.settings);
    Ok(())
}

/// Saves the nudge (positive plays earlier) and applies it at once while joined. Applied also
/// when it cannot be saved; the error is returned then.
#[tauri::command]
pub async fn set_nudge(state: State<'_, AppState>, nudge_ms: f64) -> CommandResult<Settings> {
    if !nudge_ms.is_finite() {
        return Err("The nudge must be a finite number".to_string());
    }
    let mut session = state.session.lock().await;
    session.settings.nudge_ms = nudge_ms;
    let saved = config::save(&session.settings);
    match &session.handle {
        Some(handle) => handle.set_nudge(nudge_ms),
        None => state.show_saved_nudge(&session),
    }
    saved.map_err(save_error)?;
    Ok(session.settings.clone())
}

/// Turns the core's shuffle on or off. The state shows in the status.
#[tauri::command]
pub async fn set_shuffle(state: State<'_, AppState>, on: bool) -> CommandResult<()> {
    let session = state.session.lock().await;
    let Some(handle) = &session.handle else {
        return Err("Not in a room".to_string());
    };
    handle.set_shuffle(on);
    Ok(())
}

/// Sends a room action of the window (chat, admin, permissions, another user's stream stop).
/// Refused while disconnected and for the types the core sends itself.
#[tauri::command]
pub async fn send_action(state: State<'_, AppState>, action: Value) -> CommandResult<()> {
    let session = state.session.lock().await;
    let Some(handle) = &session.handle else {
        return Err("Not in a room".to_string());
    };
    handle.send_action(action)
}

/// Uploads an audio file from `path` (the file dialog or a drop) to the joined room; the server
/// then adds it to the queue. The session is not locked during the upload, so a leave or a
/// join does not wait for it; an upload to a room that closed meanwhile fails with the
/// server's text. Errors are the texts the window shows.
#[tauri::command]
pub async fn upload_audio(state: State<'_, AppState>, path: String) -> CommandResult<()> {
    let (base, room_id) = {
        let session = state.session.lock().await;
        if session.handle.is_none() {
            return Err("Not in a room".to_string());
        }
        let base = http::http_base(&session.settings.server_url).map_err(|e| e.to_string())?;
        (base, session.settings.room_id.clone())
    };
    http::upload_audio(&http::client(), &base, &room_id, Path::new(&path))
        .await
        .map_err(|e| e.to_string())
}

/// Apps that play audio now. Blocks a worker thread for up to ~4 s.
#[tauri::command]
pub async fn list_apps(state: State<'_, AppState>) -> CommandResult<Vec<AppInfo>> {
    let backend = state.backend.clone()?;
    match tauri::async_runtime::spawn_blocking(move || backend.list_apps()).await {
        Ok(result) => result.map_err(|e| e.to_string()),
        Err(e) => Err(format!("Listing the apps failed: {e}")),
    }
}

/// Saves the selection, starts streaming it with the saved delay and returns the settings. The
/// stream starts even when the selection cannot be saved; the error is returned then.
#[tauri::command]
pub async fn start_stream(
    state: State<'_, AppState>,
    apps: Vec<String>,
) -> CommandResult<Settings> {
    if let Err(e) = &state.backend {
        return Err(e.clone());
    }
    let mut session = state.session.lock().await;
    session.settings.selected_apps = apps.clone();
    let saved = config::save(&session.settings);
    let Some(handle) = &session.handle else {
        return Err("Join a room first".to_string());
    };
    handle.start(apps, session.settings.delay_ms);
    saved.map_err(save_error)?;
    Ok(session.settings.clone())
}

/// Saves the selection, changes the streamed apps (when streaming) and returns the settings.
#[tauri::command]
pub async fn update_apps(state: State<'_, AppState>, apps: Vec<String>) -> CommandResult<Settings> {
    let mut session = state.session.lock().await;
    session.settings.selected_apps = apps.clone();
    let saved = config::save(&session.settings);
    if let Some(handle) = &session.handle {
        handle.update_apps(apps);
    }
    saved.map_err(save_error)?;
    Ok(session.settings.clone())
}

/// Stops the stream; returns when the routing is restored.
#[tauri::command]
pub async fn stop_stream(state: State<'_, AppState>) -> CommandResult<()> {
    let session = state.session.lock().await;
    if let Some(handle) = &session.handle {
        handle.stop().await;
    }
    Ok(())
}

/// Sends the current status at once and then every change, across joins. A new subscription
/// from the same webview (e.g. after a reload) replaces the old one.
#[tauri::command]
pub async fn subscribe_status(
    webview: Webview,
    state: State<'_, AppState>,
    on_status: Channel<Status>,
) -> CommandResult<()> {
    state
        .status_hub
        .subscribe(webview.label().to_string(), on_status)
}

/// Sends the current track states at once and then every change, across joins (an empty map
/// while no room is joined). A new subscription from the same webview (e.g. after a reload)
/// replaces the old one.
#[tauri::command]
pub async fn subscribe_tracks(
    webview: Webview,
    state: State<'_, AppState>,
    on_tracks: Channel<TrackStates>,
) -> CommandResult<()> {
    state
        .tracks_hub
        .subscribe(webview.label().to_string(), on_tracks)
}

/// Sends the cached room at once and then every room message, across joins. A new
/// subscription from the same webview (e.g. after a reload) replaces the old one.
#[tauri::command]
pub async fn subscribe_room(
    webview: Webview,
    state: State<'_, AppState>,
    on_message: Channel<Value>,
) -> CommandResult<()> {
    state
        .room_hub
        .subscribe(webview.label().to_string(), on_message)
}

/// Copies the values of one connection's watch to the hub until the connection ends or a new
/// generation starts. `on_value` sees each value first.
async fn pump<T: Clone + Serialize>(
    mut values: watch::Receiver<T>,
    hub: Arc<LatestHub<T>>,
    generation: u64,
    mut on_value: impl FnMut(&T),
) {
    loop {
        let current = values.borrow_and_update().clone();
        on_value(&current);
        if !hub.publish(generation, current) {
            return;
        }
        // An error means the connection has ended (shut down): its last value stays.
        if values.changed().await.is_err() {
            return;
        }
    }
}

/// Copies the room messages of one connection to the hub until the connection ends or a new
/// generation starts. Dropping the receiver then stops the core's forwarding.
async fn room_pump(
    mut feed: mpsc::UnboundedReceiver<Value>,
    hub: Arc<RoomHub<Channel<Value>>>,
    generation: u64,
) {
    while let Some(msg) = feed.recv().await {
        if !hub.publish(generation, msg) {
            return;
        }
    }
}

/// Logs each status whose [`log_line`] differs from the last one logged.
fn status_logger() -> impl FnMut(&Status) {
    let mut last_logged: Option<String> = None;
    move |status| {
        let line = log_line(status);
        if last_logged.as_ref() != Some(&line) {
            eprintln!("streamer-app: status {line}");
            last_logged = Some(line);
        }
    }
}

/// The status without the RTT, which changes every few seconds.
fn log_line(s: &Status) -> String {
    let phase = match s.phase {
        Phase::Idle => "idle".to_string(),
        Phase::Starting => "starting".to_string(),
        Phase::Streaming { stream_id } => format!("streaming({stream_id})"),
    };
    let mut line = format!(
        "connected={} synced={} listeners={} shuffle={} phase={phase}",
        s.connected, s.synced, s.listeners, s.shuffle
    );
    if let Some(message) = &s.message {
        line.push_str(&format!(" message=\"{message}\""));
    }
    line
}

/// The status while no room is joined, and until a new connection's first status.
fn idle_status(nudge_ms: f64) -> Status {
    Status {
        connected: false,
        synced: false,
        synced_once: false,
        rtt_ms: None,
        offset_ms: None,
        sync_measurements: 0,
        nudge_ms,
        shuffle: false,
        listeners: 0,
        phase: Phase::Idle,
        message: None,
    }
}

/// The latest value of the current connection (its status, its track states) and the UI
/// channels it goes to. Sends happen under the lock, so a new subscriber gets the latest value
/// first and then every change, in order.
struct LatestHub<T> {
    /// What the values are, for the error texts.
    what: &'static str,
    inner: Mutex<HubInner<T>>,
}

struct HubInner<T> {
    /// Increased for each join and leave; values of older connections are dropped.
    generation: u64,
    latest: T,
    /// By webview label.
    channels: HashMap<String, Channel<T>>,
}

impl<T: Clone + Serialize> LatestHub<T> {
    fn new(what: &'static str, initial: T) -> Self {
        Self {
            what,
            inner: Mutex::new(HubInner {
                generation: 0,
                latest: initial,
                channels: HashMap::new(),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HubInner<T>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Sends the latest value to `channel`, then registers it for the changes. A new channel
    /// of the same webview replaces the old one.
    fn subscribe(&self, label: String, channel: Channel<T>) -> CommandResult<()> {
        let mut inner = self.lock();
        channel
            .send(inner.latest.clone())
            .map_err(|e| format!("Cannot send the {}: {e}", self.what))?;
        inner.channels.insert(label, channel);
        Ok(())
    }

    /// Starts a new generation at `initial`, sends it and returns the generation.
    fn next_generation(&self, initial: T) -> u64 {
        let mut inner = self.lock();
        inner.generation += 1;
        inner.latest = initial;
        inner.broadcast(self.what);
        inner.generation
    }

    /// False when `generation` is no longer the current one (nothing changes then).
    fn publish(&self, generation: u64, value: T) -> bool {
        let mut inner = self.lock();
        if inner.generation != generation {
            return false;
        }
        inner.latest = value;
        inner.broadcast(self.what);
        true
    }
}

impl<T: Clone + Serialize> HubInner<T> {
    /// Sends the latest value to every channel; a channel that fails is dropped.
    fn broadcast(&mut self, what: &str) {
        let latest = &self.latest;
        self.channels
            .retain(|label, channel| match channel.send(latest.clone()) {
                Ok(()) => true,
                Err(e) => {
                    eprintln!("streamer-app: dropping the {what} channel of {label}: {e}");
                    false
                }
            });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use serde_json::json;
    use streamer_core::tracks::TrackState;
    use tauri::ipc::InvokeResponseBody;

    use super::*;

    #[test]
    fn the_idle_status_has_shuffle_off() {
        let status = idle_status(0.0);
        assert!(!status.shuffle);
        assert_eq!(
            serde_json::to_value(&status).unwrap()["shuffle"],
            json!(false)
        );
    }

    /// The window end of hub channels: records what they get as JSON; every send fails while
    /// `failing` is set.
    #[derive(Clone, Default)]
    struct TestWindow {
        got: Arc<Mutex<Vec<Value>>>,
        failing: Arc<AtomicBool>,
    }

    impl TestWindow {
        fn channel<T>(&self) -> Channel<T> {
            let window = self.clone();
            Channel::new(move |body| {
                if window.failing.load(Ordering::SeqCst) {
                    return Err(tauri::Error::WebviewNotFound);
                }
                let InvokeResponseBody::Json(json) = body else {
                    panic!("hub values go as JSON");
                };
                let value = serde_json::from_str(&json).expect("valid JSON");
                window.got.lock().unwrap().push(value);
                Ok(())
            })
        }

        fn got(&self) -> Vec<Value> {
            self.got.lock().unwrap().clone()
        }

        fn set_failing(&self, failing: bool) {
            self.failing.store(failing, Ordering::SeqCst);
        }
    }

    fn tracks_hub() -> LatestHub<TrackStates> {
        LatestHub::new("track states", TrackStates::new())
    }

    fn ready(url: &str, duration_s: f64) -> TrackStates {
        TrackStates::from([(url.to_string(), TrackState::Ready { duration_s })])
    }

    fn ready_json(url: &str, duration_s: f64) -> Value {
        json!({ url: { "state": "ready", "durationS": duration_s } })
    }

    #[test]
    fn subscribe_gets_the_latest_value_then_every_change() {
        // A reloaded window shows the current state at once and then follows the changes: a
        // gap or a repeat between the two shows a wrong state.
        let hub = tracks_hub();
        let generation = hub.next_generation(TrackStates::new());
        assert!(hub.publish(generation, ready("a.mp3", 2.0)));

        let window = TestWindow::default();
        hub.subscribe("main".to_string(), window.channel()).unwrap();
        assert_eq!(window.got(), vec![ready_json("a.mp3", 2.0)]);

        assert!(hub.publish(generation, ready("b.mp3", 3.0)));
        assert_eq!(
            window.got(),
            vec![ready_json("a.mp3", 2.0), ready_json("b.mp3", 3.0)]
        );
    }

    #[test]
    fn a_join_starts_the_tracks_hub_empty() {
        // After a leave or a new join, the queue must not show the load states of the old
        // room's tracks.
        let hub = tracks_hub();
        let generation = hub.next_generation(TrackStates::new());
        assert!(hub.publish(generation, ready("a.mp3", 2.0)));
        let window = TestWindow::default();
        hub.subscribe("main".to_string(), window.channel()).unwrap();

        hub.next_generation(TrackStates::new());
        assert_eq!(
            window.got(),
            vec![ready_json("a.mp3", 2.0), json!({})],
            "sent at once"
        );

        let later = TestWindow::default();
        hub.subscribe("other".to_string(), later.channel()).unwrap();
        assert_eq!(later.got(), vec![json!({})], "the latest value");
    }

    #[test]
    fn values_of_an_old_generation_are_dropped() {
        // The old connection still sends after a new join (at least the empty map of its
        // shutdown): it must not replace the new room's states.
        let hub = tracks_hub();
        let old = hub.next_generation(TrackStates::new());
        let current = hub.next_generation(TrackStates::new());
        assert!(hub.publish(current, ready("a.mp3", 2.0)));
        let window = TestWindow::default();
        hub.subscribe("main".to_string(), window.channel()).unwrap();

        assert!(!hub.publish(old, TrackStates::new()));
        assert_eq!(window.got(), vec![ready_json("a.mp3", 2.0)], "not sent");

        let later = TestWindow::default();
        hub.subscribe("other".to_string(), later.channel()).unwrap();
        assert_eq!(
            later.got(),
            vec![ready_json("a.mp3", 2.0)],
            "not the latest value"
        );
    }

    #[test]
    fn a_failing_channel_is_dropped() {
        let hub = tracks_hub();
        let generation = hub.next_generation(TrackStates::new());
        let healthy = TestWindow::default();
        let broken = TestWindow::default();
        hub.subscribe("healthy".to_string(), healthy.channel())
            .unwrap();
        hub.subscribe("broken".to_string(), broken.channel())
            .unwrap();

        broken.set_failing(true);
        assert!(hub.publish(generation, ready("a.mp3", 2.0)));
        broken.set_failing(false);
        assert!(hub.publish(generation, ready("b.mp3", 3.0)));
        assert_eq!(
            healthy.got(),
            vec![
                json!({}),
                ready_json("a.mp3", 2.0),
                ready_json("b.mp3", 3.0)
            ]
        );
        assert_eq!(
            broken.got(),
            vec![json!({})],
            "dropped at the first failure"
        );

        // A channel that cannot take the latest value is refused.
        let refused = TestWindow::default();
        refused.set_failing(true);
        assert!(
            hub.subscribe("refused".to_string(), refused.channel())
                .is_err()
        );
        refused.set_failing(false);
        assert!(hub.publish(generation, ready("c.mp3", 4.0)));
        assert_eq!(refused.got(), Vec::<Value>::new());
    }

    #[test]
    fn join_rejects_a_bad_room_code_or_server_url() {
        // Checked before anything connects: a wrong address must show on the join screen, not
        // as a connection that never comes up.
        let server = "ws://localhost:8091/ws";
        for code in ["12345", "12a456", "1234567", ""] {
            assert_eq!(
                validate_join(code, "Ann", server),
                Err("Room code must be 6 digits".to_string()),
                "{code:?}"
            );
        }
        for name in ["", "   "] {
            assert_eq!(
                validate_join("123456", name, server),
                Err("Enter a name".to_string()),
                "{name:?}"
            );
        }
        assert_eq!(
            validate_join("123456", "Ann", "http://x/ws"),
            Err("unsupported URL scheme: http".to_string())
        );
        assert!(
            validate_join("123456", "Ann", "not a url")
                .unwrap_err()
                .starts_with("invalid server URL")
        );
        assert_eq!(validate_join("123456", "Ann", server), Ok(()));
    }
}
