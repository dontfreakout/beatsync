//! Helpers shared by the unit tests.

use std::path::PathBuf;

/// A fresh directory under the system temp dir, removed on drop (also when a test fails).
pub(crate) struct TempDir(pub(crate) PathBuf);

impl TempDir {
    pub(crate) fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "beatsync-streamer-{name}-{}-{}",
            std::process::id(),
            crate::clock::mono_ns()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    /// The names of the entries, sorted; none when the directory is gone.
    pub(crate) fn files(&self) -> Vec<String> {
        let entries = match std::fs::read_dir(&self.0) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(e) => panic!("cannot list {}: {e}", self.0.display()),
        };
        let mut names: Vec<String> = entries
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
