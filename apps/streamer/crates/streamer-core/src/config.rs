//! Persisted settings (spec §9.7): one JSON file in the app config directory, shared by the
//! CLI and the Tauri app.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::frame::{DEFAULT_DELAY_MS, MAX_DELAY_MS, MIN_DELAY_MS};

/// The Tauri identifier; Tauri's app config dir is `<config dir>/<identifier>`.
pub const APP_IDENTIFIER: &str = "app.beatsync.streamer";
pub const DEFAULT_SERVER_URL: &str = "ws://localhost:8080/ws";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub server_url: String,
    pub room_id: String,
    pub username: String,
    /// UUID v4, generated once and kept (spec §6.3).
    pub client_id: String,
    pub selected_apps: Vec<String>,
    pub delay_ms: u32,
    /// Shifts the local copy, e.g. for an output delay that PipeWire under-reports.
    pub local_offset_ms: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            server_url: DEFAULT_SERVER_URL.to_string(),
            room_id: String::new(),
            username: String::new(),
            client_id: String::new(),
            selected_apps: Vec::new(),
            delay_ms: DEFAULT_DELAY_MS,
            local_offset_ms: 0.0,
        }
    }
}

/// `<config dir>/app.beatsync.streamer/config.json`.
pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_IDENTIFIER)
        .join("config.json")
}

/// Loads the settings; a missing or unreadable file gives the defaults. Generates and saves
/// the `client_id` when there is none yet.
pub fn load_or_create() -> Settings {
    load_or_create_at(&config_path())
}

/// Writes the settings to [`config_path`].
pub fn save(settings: &Settings) -> io::Result<()> {
    save_at(&config_path(), settings)
}

impl Settings {
    /// Clamps the delay to 250..5000 ms and replaces a non-finite local offset.
    pub fn normalize(&mut self) {
        self.delay_ms = self.delay_ms.clamp(MIN_DELAY_MS, MAX_DELAY_MS);
        if !self.local_offset_ms.is_finite() {
            self.local_offset_ms = 0.0;
        }
    }
}

pub fn load_or_create_at(path: &Path) -> Settings {
    let mut settings = match std::fs::read_to_string(path) {
        Ok(json) => serde_json::from_str(&json).unwrap_or_else(|e| {
            eprintln!(
                "streamer: ignoring unreadable settings {}: {e}",
                path.display()
            );
            Settings::default()
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Settings::default(),
        Err(e) => {
            eprintln!("streamer: cannot read settings {}: {e}", path.display());
            Settings::default()
        }
    };
    settings.normalize();
    if settings.client_id.is_empty() {
        settings.client_id = uuid::Uuid::new_v4().to_string();
        if let Err(e) = save_at(path, &settings) {
            eprintln!("streamer: cannot save settings {}: {e}", path.display());
        }
    }
    settings
}

/// Writes through a temporary file and a rename, so a crash never leaves half a file.
pub fn save_at(path: &Path, settings: &Settings) -> io::Result<()> {
    let mut settings = settings.clone();
    settings.normalize();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string_pretty(&settings).map_err(io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory under the system temp dir, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "beatsync-streamer-{name}-{}-{}",
                std::process::id(),
                crate::clock::mono_ns()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn config_round_trip_and_client_id_stable() {
        // A new clientId on every start would make the server see a new client each time.
        let dir = TempDir::new("config");
        let path = dir.0.join("nested").join("config.json");
        let first = load_or_create_at(&path);
        let id = uuid::Uuid::parse_str(&first.client_id).expect("client id is a UUID");
        assert_eq!(id.get_version_num(), 4);
        assert!(path.exists(), "the generated client id is saved");
        assert_eq!(load_or_create_at(&path).client_id, first.client_id);

        let edited = Settings {
            server_url: "wss://beatsync.example/ws".to_string(),
            room_id: "123456".to_string(),
            username: "Laptop".to_string(),
            selected_apps: vec!["Spotify".to_string(), "Firefox".to_string()],
            delay_ms: 1500,
            local_offset_ms: -12.5,
            ..first.clone()
        };
        save_at(&path, &edited).unwrap();
        assert_eq!(load_or_create_at(&path), edited);

        // The Tauri UI reads the same file with camelCase keys.
        let json = std::fs::read_to_string(&path).unwrap();
        for key in [
            "serverUrl",
            "roomId",
            "username",
            "clientId",
            "selectedApps",
            "delayMs",
            "localOffsetMs",
        ] {
            assert!(json.contains(&format!("\"{key}\"")), "{key} in {json}");
        }
    }

    #[test]
    fn delay_is_clamped() {
        let dir = TempDir::new("clamp");
        let path = dir.0.join("config.json");
        let cases = [(10, MIN_DELAY_MS), (99_999, MAX_DELAY_MS), (1200, 1200)];
        for (stored, loaded) in cases {
            let json = format!(r#"{{"clientId":"x","roomId":"123456","delayMs":{stored}}}"#);
            std::fs::write(&path, json).unwrap();
            let settings = load_or_create_at(&path);
            assert_eq!(settings.delay_ms, loaded);
            // Missing keys take their defaults.
            assert_eq!(settings.server_url, DEFAULT_SERVER_URL);
            assert_eq!(settings.room_id, "123456");
        }
        let too_long = Settings {
            client_id: "x".to_string(),
            delay_ms: 60_000,
            ..Settings::default()
        };
        save_at(&path, &too_long).unwrap();
        assert_eq!(load_or_create_at(&path).delay_ms, MAX_DELAY_MS);
    }
}
