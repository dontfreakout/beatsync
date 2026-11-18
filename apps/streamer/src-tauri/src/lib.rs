//! Beatsync Streamer: a Tauri shell around `streamer_core::runtime` (spec §9).
//!
//! Every way out (closing the window, `app.exit`, SIGINT/SIGTERM) goes through
//! [`RunEvent::ExitRequested`], which stops the stream and restores the routing before the
//! process ends.

mod commands;

use std::sync::atomic::Ordering;
use std::time::Duration;

use tauri::{AppHandle, ExitRequestApi, Manager, RunEvent};

use commands::AppState;

/// The exit cleanup gives up after this long (a hung audio backend). PipeWire then removes the
/// sink with the process, and WirePlumber moves the streams back. Above the core's worst case
/// of about 12 s: a start in progress, a queued app update and the stop, up to 4 s each.
const EXIT_CLEANUP_TIMEOUT: Duration = Duration::from_secs(15);

pub fn run() {
    let app = tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            commands::get_settings,
            commands::save_settings,
            commands::list_apps,
            commands::start_stream,
            commands::update_apps,
            commands::stop_stream,
            commands::subscribe_status,
        ])
        .setup(|app| {
            let state = AppState::new();
            tauri::async_runtime::block_on(state.connect());
            app.manage(state);
            watch_signals(app.handle().clone());
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("cannot build the Beatsync Streamer app");

    app.run(|app, event| {
        if let RunEvent::ExitRequested { code, api, .. } = event {
            on_exit_requested(app, code, &api);
        }
    });
}

/// Holds the exit until the stream is stopped and the routing restored, then exits again.
fn on_exit_requested(app: &AppHandle, code: Option<i32>, api: &ExitRequestApi) {
    let state = app.state::<AppState>();
    if state.exit_done.load(Ordering::SeqCst) {
        return;
    }
    api.prevent_exit();
    if state.exit_started.swap(true, Ordering::SeqCst) {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        if tokio::time::timeout(EXIT_CLEANUP_TIMEOUT, state.shutdown())
            .await
            .is_err()
        {
            eprintln!("streamer-app: the stop did not finish in time; exiting without it");
        }
        state.exit_done.store(true, Ordering::SeqCst);
        app.exit(code.unwrap_or(0));
    });
}

/// SIGINT/SIGTERM exit the app the normal way (stream stopped, routing restored). A second
/// signal during that exits at once with 130/143, like the CLI.
#[cfg(unix)]
fn watch_signals(app: AppHandle) {
    use tokio::signal::unix::{SignalKind, signal};

    tauri::async_runtime::spawn(async move {
        let (mut sigint, mut sigterm) = match (
            signal(SignalKind::interrupt()),
            signal(SignalKind::terminate()),
        ) {
            (Ok(int), Ok(term)) => (int, term),
            (Err(e), _) | (_, Err(e)) => {
                eprintln!("streamer-app: cannot watch signals: {e}");
                return;
            }
        };
        tokio::select! {
            _ = sigint.recv() => {}
            _ = sigterm.recv() => {}
        }
        eprintln!("streamer-app: signal received, exiting (send it again to exit at once)");
        app.exit(0);
        let code = tokio::select! {
            _ = sigint.recv() => 130,
            _ = sigterm.recv() => 143,
        };
        eprintln!("streamer-app: exiting without restoring the routing");
        std::process::exit(code);
    });
}

#[cfg(not(unix))]
fn watch_signals(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            app.exit(0);
        }
    });
}
