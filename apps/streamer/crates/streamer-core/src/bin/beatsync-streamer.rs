//! `beatsync-streamer`: streams the audio of local apps into a beatsync room (spec §9).
//!
//! Flags override the saved settings and are saved. Prints one status line per change.
//! SIGINT/SIGTERM stop the stream, restore the routing and exit 0; a second signal exits
//! at once (PipeWire then removes the sink and WirePlumber moves the streams back).

use std::fmt::Write as _;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use tokio::signal::unix::{SignalKind, signal};

use streamer_core::audio::{AppInfo, AudioBackend, AudioError};
use streamer_core::config::{self, Settings};
use streamer_core::connection::build_ws_url;
use streamer_core::engine::{Phase, Status};
use streamer_core::runtime::StreamerHandle;

#[derive(Parser, Debug)]
#[command(
    name = "beatsync-streamer",
    version,
    about = "Streams the audio of local apps into a beatsync room"
)]
struct Args {
    /// Server WebSocket URL, e.g. ws://localhost:8080/ws
    #[arg(long, value_name = "URL")]
    server: Option<String>,
    /// Room code
    #[arg(long, value_name = "CODE")]
    room: Option<String>,
    /// Name shown in the room
    #[arg(long, value_name = "NAME")]
    name: Option<String>,
    /// App to stream (repeatable); replaces the saved selection
    #[arg(long = "app", value_name = "NAME")]
    apps: Vec<String>,
    /// Room delay in ms (250..5000)
    #[arg(long, value_name = "MS")]
    delay: Option<u32>,
    /// Shifts the local copy on this machine, in ms
    #[arg(long, value_name = "MS", allow_negative_numbers = true)]
    local_offset: Option<f64>,
    /// Lists the apps that play audio now and exits
    #[arg(long)]
    list_apps: bool,
}

const USAGE_ERROR: u8 = 2;

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    let mut settings = config::load_or_create();
    apply(&args, &mut settings);
    if let Err(e) = config::save(&settings) {
        eprintln!(
            "beatsync-streamer: cannot save the settings to {}: {e}",
            config::config_path().display()
        );
    }
    if !args.list_apps
        && let Err(message) = check_ready(&settings)
    {
        eprintln!("beatsync-streamer: {message}");
        return ExitCode::from(USAGE_ERROR);
    }

    let backend = match new_backend().await {
        Ok(backend) => backend,
        Err(e) => {
            // UnsupportedVersions prints exactly the required-versions message.
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let code = if args.list_apps {
        list_apps(backend.clone()).await
    } else {
        stream(settings, backend.clone()).await
    };
    // Dropping the backend joins its thread.
    let _ = tokio::task::spawn_blocking(move || drop(backend)).await;
    code
}

fn apply(args: &Args, settings: &mut Settings) {
    if let Some(server) = &args.server {
        settings.server_url = server.clone();
    }
    if let Some(room) = &args.room {
        settings.room_id = room.clone();
    }
    if let Some(name) = &args.name {
        settings.username = name.clone();
    }
    if !args.apps.is_empty() {
        settings.selected_apps = args.apps.clone();
    }
    if let Some(delay) = args.delay {
        settings.delay_ms = delay;
    }
    if let Some(offset) = args.local_offset {
        settings.local_offset_ms = offset;
    }
    settings.normalize();
}

fn check_ready(settings: &Settings) -> Result<(), String> {
    if settings.selected_apps.is_empty() {
        return Err("no app selected: pass --app <name> (see --list-apps)".to_string());
    }
    if settings.room_id.is_empty() {
        return Err("no room: pass --room <code>".to_string());
    }
    if settings.username.is_empty() {
        return Err("no name: pass --name <name>".to_string());
    }
    build_ws_url(
        &settings.server_url,
        &settings.room_id,
        &settings.username,
        &settings.client_id,
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}

#[cfg(target_os = "linux")]
async fn new_backend() -> Result<Arc<dyn AudioBackend>, AudioError> {
    use streamer_core::audio::pipewire::PipeWireBackend;
    match tokio::task::spawn_blocking(PipeWireBackend::new).await {
        Ok(result) => result.map(|backend| Arc::new(backend) as Arc<dyn AudioBackend>),
        Err(e) => Err(AudioError::Backend(format!(
            "Cannot start the audio backend: {e}"
        ))),
    }
}

#[cfg(not(target_os = "linux"))]
async fn new_backend() -> Result<Arc<dyn AudioBackend>, AudioError> {
    Err(AudioError::Unavailable(
        "only Linux with PipeWire is supported".to_string(),
    ))
}

async fn list_apps(backend: Arc<dyn AudioBackend>) -> ExitCode {
    let apps = match tokio::task::spawn_blocking(move || backend.list_apps()).await {
        Ok(Ok(apps)) => apps,
        Ok(Err(e)) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("beatsync-streamer: listing the apps failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    if apps.is_empty() {
        println!("No app plays audio now.");
    }
    for app in &apps {
        println!("{}", app_line(app));
    }
    ExitCode::SUCCESS
}

fn app_line(app: &AppInfo) -> String {
    let mut line = format!(
        "{}  binary={}  streams={}",
        app.name,
        app.binary.as_deref().unwrap_or("-"),
        app.streams
    );
    if app.is_browser {
        line.push_str("  browser");
    }
    line
}

async fn stream(settings: Settings, backend: Arc<dyn AudioBackend>) -> ExitCode {
    let (mut sigint, mut sigterm) = match (
        signal(SignalKind::interrupt()),
        signal(SignalKind::terminate()),
    ) {
        (Ok(int), Ok(term)) => (int, term),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("beatsync-streamer: cannot watch signals: {e}");
            return ExitCode::FAILURE;
        }
    };
    let apps = settings.selected_apps.clone();
    let delay_ms = settings.delay_ms;
    let handle = StreamerHandle::connect(settings, backend).await;
    let mut status = handle.status();
    handle.start(apps, delay_ms);

    let mut last = status_line(&status.borrow_and_update());
    println!("{last}");
    loop {
        tokio::select! {
            _ = sigint.recv() => break,
            _ = sigterm.recv() => break,
            changed = status.changed() => {
                if changed.is_err() {
                    // No engine (invalid server URL): the last status says why.
                    break;
                }
                let line = status_line(&status.borrow_and_update());
                if line != last {
                    println!("{line}");
                    last = line;
                }
            }
        }
    }
    eprintln!("beatsync-streamer: stopping (press Ctrl-C again to exit at once)");
    tokio::select! {
        _ = handle.shutdown() => ExitCode::SUCCESS,
        _ = sigint.recv() => ExitCode::from(130),
        _ = sigterm.recv() => ExitCode::from(143),
    }
}

/// E.g. `connected synced rtt=24ms listeners=3 phase=streaming(7)`.
fn status_line(s: &Status) -> String {
    let mut line = String::new();
    line.push_str(if s.connected {
        "connected"
    } else {
        "disconnected"
    });
    line.push_str(if s.synced { " synced" } else { " syncing" });
    match s.rtt_ms {
        Some(rtt) => {
            let _ = write!(line, " rtt={rtt:.0}ms");
        }
        None => line.push_str(" rtt=-"),
    }
    let _ = write!(line, " listeners={}", s.listeners);
    match s.phase {
        Phase::Idle => line.push_str(" phase=idle"),
        Phase::Starting => line.push_str(" phase=starting"),
        Phase::Streaming { stream_id } => {
            let _ = write!(line, " phase=streaming({stream_id})");
        }
    }
    if let Some(message) = &s.message {
        let _ = write!(line, " message=\"{message}\"");
    }
    line
}
