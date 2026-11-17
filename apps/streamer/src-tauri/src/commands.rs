//! Tauri commands and the app state behind them (spec §9.7).
//!
//! The state owns the settings, the audio backend and the connection ([`StreamerHandle`]).
//! A connection exists while the room and the name are set and the backend works; it is
//! replaced when a setting it reads at connect time changes. Statuses reach the UI through
//! [`StatusHub`], which outlives the connections.

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tauri::ipc::Channel;
use tauri::{State, Webview};
use tokio::sync::watch;

use streamer_core::audio::{AppInfo, AudioBackend};
use streamer_core::config::{self, Settings};
use streamer_core::engine::{Phase, Status};
use streamer_core::runtime::StreamerHandle;

pub type CommandResult<T> = Result<T, String>;

pub struct AppState {
    /// Kept for the app's lifetime: dropping the PipeWire backend joins its thread, which must
    /// not happen on a runtime worker. `Err` is the creation error that the UI shows in place
    /// of the app list (e.g. `PipeWire 1.0 and WirePlumber 0.5 or later required`).
    backend: Result<Arc<dyn AudioBackend>, String>,
    session: tokio::sync::Mutex<Session>,
    hub: Arc<StatusHub>,
    /// Set once the exit cleanup has started.
    pub exit_started: AtomicBool,
    /// Set once the exit cleanup has finished: the next exit request goes through.
    pub exit_done: AtomicBool,
}

struct Session {
    settings: Settings,
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
        Self {
            backend,
            session: tokio::sync::Mutex::new(Session {
                settings,
                handle: None,
                closed: false,
            }),
            hub: Arc::new(StatusHub::new()),
            exit_started: AtomicBool::new(false),
            exit_done: AtomicBool::new(false),
        }
    }

    /// Connects with the loaded settings.
    pub async fn connect(&self) {
        let mut session = self.session.lock().await;
        self.reconnect(&mut session).await;
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

    /// Replaces the connection with one for the current settings (none while the room or the
    /// name is empty, or without a backend).
    async fn reconnect(&self, session: &mut Session) {
        if let Some(old) = session.handle.take() {
            old.shutdown().await;
        }
        let generation = self.hub.next_generation();
        if session.closed {
            return;
        }
        let Ok(backend) = &self.backend else { return };
        let settings = &session.settings;
        if settings.room_id.is_empty() || settings.username.is_empty() {
            return;
        }
        eprintln!(
            "streamer-app: connecting to {} room {} as {}",
            settings.server_url, settings.room_id, settings.username
        );
        let handle = StreamerHandle::connect(settings.clone(), backend.clone()).await;
        tauri::async_runtime::spawn(pump(handle.status(), self.hub.clone(), generation));
        session.handle = Some(handle);
    }
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

/// The connection reads these only when it connects.
fn needs_reconnect(old: &Settings, new: &Settings) -> bool {
    old.server_url != new.server_url
        || old.room_id != new.room_id
        || old.username != new.username
        || old.local_offset_ms != new.local_offset_ms
}

fn save_error(e: std::io::Error) -> String {
    format!(
        "Cannot save the settings to {}: {e}",
        config::config_path().display()
    )
}

#[tauri::command]
pub async fn get_settings(state: State<'_, AppState>) -> CommandResult<Settings> {
    Ok(state.session.lock().await.settings.clone())
}

/// Saves the settings (the `clientId` stays) and returns them normalized. Reconnects when the
/// server, the room, the name or the local offset changed; a running stream stops then.
/// Nothing changes when the file cannot be written.
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
    let reconnect = needs_reconnect(&session.settings, &next);
    session.settings = next;
    if reconnect {
        state.reconnect(&mut session).await;
    }
    Ok(session.settings.clone())
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

/// Saves the selection and starts streaming it with the saved delay. The stream starts even
/// when the selection cannot be saved; the error is returned then.
#[tauri::command]
pub async fn start_stream(state: State<'_, AppState>, apps: Vec<String>) -> CommandResult<()> {
    if let Err(e) = &state.backend {
        return Err(e.clone());
    }
    let mut session = state.session.lock().await;
    session.settings.selected_apps = apps.clone();
    let saved = config::save(&session.settings);
    let Some(handle) = &session.handle else {
        return Err("Enter a room and a name first".to_string());
    };
    handle.start(apps, session.settings.delay_ms);
    saved.map_err(save_error)
}

/// Saves the selection and changes the streamed apps (when streaming).
#[tauri::command]
pub async fn update_apps(state: State<'_, AppState>, apps: Vec<String>) -> CommandResult<()> {
    let mut session = state.session.lock().await;
    session.settings.selected_apps = apps.clone();
    let saved = config::save(&session.settings);
    if let Some(handle) = &session.handle {
        handle.update_apps(apps);
    }
    saved.map_err(save_error)
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

/// Sends the current status at once and then every change, across reconnects. A new
/// subscription from the same webview (e.g. after a reload) replaces the old one.
#[tauri::command]
pub async fn subscribe_status(
    webview: Webview,
    state: State<'_, AppState>,
    on_status: Channel<Status>,
) -> CommandResult<()> {
    state.hub.subscribe(webview.label().to_string(), on_status)
}

/// Copies the statuses of one connection to the hub until the connection ends or is replaced.
async fn pump(mut status: watch::Receiver<Status>, hub: Arc<StatusHub>, generation: u64) {
    let mut last_logged: Option<String> = None;
    loop {
        let current = status.borrow_and_update().clone();
        let line = log_line(&current);
        if last_logged.as_ref() != Some(&line) {
            eprintln!("streamer-app: status {line}");
            last_logged = Some(line);
        }
        if !hub.publish(generation, current) {
            return;
        }
        // An error means the engine has ended (shut down, or an invalid server URL from the
        // start): its last status stays.
        if status.changed().await.is_err() {
            return;
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
        "connected={} synced={} listeners={} phase={phase}",
        s.connected, s.synced, s.listeners
    );
    if let Some(message) = &s.message {
        line.push_str(&format!(" message=\"{message}\""));
    }
    line
}

fn idle_status() -> Status {
    Status {
        connected: false,
        synced: false,
        rtt_ms: None,
        listeners: 0,
        phase: Phase::Idle,
        message: None,
    }
}

/// The latest status of the current connection and the UI channels it goes to. Sends happen
/// under the lock, so a new subscriber gets the latest status first and then every change, in
/// order.
struct StatusHub {
    inner: Mutex<HubInner>,
}

struct HubInner {
    /// Increased for each new connection; statuses of older ones are dropped.
    generation: u64,
    latest: Status,
    /// By webview label.
    channels: HashMap<String, Channel<Status>>,
}

impl StatusHub {
    fn new() -> Self {
        Self {
            inner: Mutex::new(HubInner {
                generation: 0,
                latest: idle_status(),
                channels: HashMap::new(),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HubInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn subscribe(&self, label: String, channel: Channel<Status>) -> CommandResult<()> {
        let mut inner = self.lock();
        channel
            .send(inner.latest.clone())
            .map_err(|e| format!("Cannot send the status: {e}"))?;
        inner.channels.insert(label, channel);
        Ok(())
    }

    /// Starts a new generation at the idle status and returns it.
    fn next_generation(&self) -> u64 {
        let mut inner = self.lock();
        inner.generation += 1;
        inner.latest = idle_status();
        inner.broadcast();
        inner.generation
    }

    /// False when `generation` is no longer the current one.
    fn publish(&self, generation: u64, status: Status) -> bool {
        let mut inner = self.lock();
        if inner.generation != generation {
            return false;
        }
        inner.latest = status;
        inner.broadcast();
        true
    }
}

impl HubInner {
    /// Sends the latest status to every channel; a channel that fails is dropped.
    fn broadcast(&mut self) {
        let latest = &self.latest;
        self.channels
            .retain(|label, channel| match channel.send(latest.clone()) {
                Ok(()) => true,
                Err(e) => {
                    eprintln!("streamer-app: dropping the status channel of {label}: {e}");
                    false
                }
            });
    }
}
