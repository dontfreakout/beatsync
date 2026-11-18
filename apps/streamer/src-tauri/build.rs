/// The app's commands. Each gets an `allow-…` permission; `capabilities/default.json` grants
/// them to the main window (a command missing there is refused at run time).
const COMMANDS: &[&str] = &[
    "get_settings",
    "save_settings",
    "list_apps",
    "start_stream",
    "update_apps",
    "stop_stream",
    "subscribe_status",
];

fn main() {
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .app_manifest(tauri_build::AppManifest::new().commands(COMMANDS)),
    )
    .expect("failed to run tauri-build");
}
