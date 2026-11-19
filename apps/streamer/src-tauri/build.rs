/// The app's commands. Each gets an `allow-…` permission; `capabilities/default.json` grants
/// them to the main window (a command missing there is refused at run time).
const COMMANDS: &[&str] = &[
    "get_session",
    "save_settings",
    "join_room",
    "leave_room",
    "set_nudge",
    "set_shuffle",
    "send_action",
    "upload_audio",
    "list_apps",
    "start_stream",
    "update_apps",
    "stop_stream",
    "subscribe_status",
    "subscribe_tracks",
    "subscribe_room",
];

fn main() {
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .app_manifest(tauri_build::AppManifest::new().commands(COMMANDS)),
    )
    .expect("failed to run tauri-build");
}
