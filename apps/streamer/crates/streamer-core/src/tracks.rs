//! Load states of the queue tracks, and the planner that decides which track to download
//! (spec C8, call K1). Pure: no I/O, no clock. The loader in [`crate::runtime`] does the
//! downloads and reports back.

use std::collections::{BTreeMap, HashSet, VecDeque};

use serde::Serialize;
use serde_json::Value;
use url::Url;

/// Downloads that run at once; more wait in arrival order.
pub const MAX_DOWNLOADS: usize = 2;

/// The load state of one track, as the window reads it: `{"state":"loading"}`,
/// `{"state":"ready","durationS":2.0}`, `{"state":"error","message":"…"}`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
    tag = "state",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum TrackState {
    /// Waiting for a download slot, downloading, or reading the duration.
    Loading,
    /// Downloaded, and the duration is known.
    Ready { duration_s: f64 },
    /// The text the queue shows for the track.
    Error { message: String },
}

/// The tracks that became current, by URL as the server sends it.
pub type TrackStates = BTreeMap<String, TrackState>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackAction {
    /// Download the track and read its duration, then report with [`TrackPlanner::on_done`].
    Download { url: String },
    /// The URL left the queue: stop its download and delete its file.
    Forget { url: String },
}

/// Decides from the room messages which tracks to download (K1): a track downloads once it
/// becomes the current track, and its file stays while its URL is in the queue.
#[derive(Debug, Default)]
pub struct TrackPlanner {
    states: TrackStates,
    /// The downloads that run, at most [`MAX_DOWNLOADS`].
    running: Vec<String>,
    /// Current tracks that wait for a download slot, in arrival order.
    waiting: VecDeque<String>,
}

impl TrackPlanner {
    /// A URL becomes current through `SET_AUDIO_SOURCES.currentAudioSource`,
    /// `LOAD_AUDIO_SOURCE` or a scheduled `PLAY`/`PAUSE`; `SET_AUDIO_SOURCES` forgets every
    /// URL that left the queue.
    pub fn on_room_message(&mut self, msg: &Value) -> Vec<TrackAction> {
        let mut actions = Vec::new();
        match msg["type"].as_str() {
            Some("ROOM_EVENT") => {
                let event = &msg["event"];
                match event["type"].as_str() {
                    Some("SET_AUDIO_SOURCES") => {
                        if let Some(sources) = event["sources"].as_array() {
                            let queue: HashSet<&str> =
                                sources.iter().filter_map(|s| s["url"].as_str()).collect();
                            self.forget_all_but(&queue, &mut actions);
                            // A deleted track that was paused stays the server's current
                            // source: no download for a URL outside the queue.
                            let current = event["currentAudioSource"].as_str();
                            self.make_current(current.filter(|url| queue.contains(url)), false);
                        }
                    }
                    Some("LOAD_AUDIO_SOURCE") => {
                        // The only message that retries a failed track (as the web does).
                        self.make_current(event["audioSourceToPlay"]["url"].as_str(), true);
                    }
                    _ => {}
                }
            }
            Some("SCHEDULED_ACTION") => {
                let action = &msg["scheduledAction"];
                if matches!(action["type"].as_str(), Some("PLAY" | "PAUSE")) {
                    self.make_current(action["audioSource"].as_str(), false);
                }
            }
            _ => {}
        }
        self.start_waiting(&mut actions);
        actions
    }

    /// The result of a [`TrackAction::Download`]: the duration in seconds, or the error text.
    /// A result for a URL that was forgotten meanwhile changes nothing.
    pub fn on_done(&mut self, url: &str, result: Result<f64, String>) -> Vec<TrackAction> {
        let Some(index) = self.running.iter().position(|u| u == url) else {
            return Vec::new();
        };
        self.running.remove(index);
        let state = match result {
            Ok(duration_s) => TrackState::Ready { duration_s },
            Err(message) => TrackState::Error { message },
        };
        self.states.insert(url.to_string(), state);
        let mut actions = Vec::new();
        self.start_waiting(&mut actions);
        actions
    }

    pub fn states(&self) -> &TrackStates {
        &self.states
    }

    fn forget_all_but(&mut self, queue: &HashSet<&str>, actions: &mut Vec<TrackAction>) {
        let gone: Vec<String> = self
            .states
            .keys()
            .filter(|url| !queue.contains(url.as_str()))
            .cloned()
            .collect();
        for url in gone {
            self.states.remove(&url);
            self.running.retain(|u| *u != url);
            self.waiting.retain(|u| *u != url);
            actions.push(TrackAction::Forget { url });
        }
    }

    /// A URL with a state is loading or done already. An error stays until the URL leaves the
    /// queue, except that `retry_error` (`LOAD_AUDIO_SOURCE`) loads it again.
    fn make_current(&mut self, url: Option<&str>, retry_error: bool) {
        let Some(url) = url.filter(|url| !url.is_empty()) else {
            return;
        };
        match self.states.get(url) {
            Some(TrackState::Error { .. }) if retry_error => {}
            Some(_) => return,
            None => {}
        }
        self.states.insert(url.to_string(), TrackState::Loading);
        self.waiting.push_back(url.to_string());
    }

    fn start_waiting(&mut self, actions: &mut Vec<TrackAction>) {
        while self.running.len() < MAX_DOWNLOADS
            && let Some(url) = self.waiting.pop_front()
        {
            self.running.push(url.clone());
            actions.push(TrackAction::Download { url });
        }
    }
}

/// The cache file name of a track: 16 hex digits of the 64-bit FNV-1a hash of the URL, so
/// each URL has its own file, and the extension of the URL path (`.bin` without one) as the
/// format hint for the duration probe.
pub fn track_file_name(url: &str) -> String {
    let hash = url.bytes().fold(FNV_OFFSET_BASIS, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(FNV_PRIME)
    });
    let extension = extension(url).unwrap_or_else(|| "bin".to_string());
    format!("{hash:016x}.{extension}")
}

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// The extension of the last path segment, lowercase. Only a short ASCII word counts: it
/// becomes part of a file name in the cache dir.
fn extension(url: &str) -> Option<String> {
    // A server path (demo mode) resolves against any base; only the path is used.
    let base = Url::parse("http://localhost/").ok()?;
    let url = base.join(url).ok()?;
    let name = url.path_segments()?.next_back()?;
    let (_, extension) = name.rsplit_once('.')?;
    let plain =
        (1..=8).contains(&extension.len()) && extension.bytes().all(|b| b.is_ascii_alphanumeric());
    plain.then(|| extension.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn queue(urls: &[&str], current: Option<&str>) -> Value {
        let sources: Vec<Value> = urls.iter().map(|url| json!({ "url": url })).collect();
        let mut event = json!({ "type": "SET_AUDIO_SOURCES", "sources": sources });
        if let Some(current) = current {
            event["currentAudioSource"] = json!(current);
        }
        json!({ "type": "ROOM_EVENT", "event": event })
    }

    fn load(url: &str) -> Value {
        json!({
            "type": "ROOM_EVENT",
            "event": { "type": "LOAD_AUDIO_SOURCE", "audioSourceToPlay": { "url": url } },
        })
    }

    fn scheduled(kind: &str, url: &str) -> Value {
        json!({
            "type": "SCHEDULED_ACTION",
            "serverTimeToExecute": 1000,
            "scheduledAction": { "type": kind, "audioSource": url, "trackTimeSeconds": 0 },
        })
    }

    fn download(url: &str) -> TrackAction {
        TrackAction::Download {
            url: url.to_string(),
        }
    }

    fn forget(url: &str) -> TrackAction {
        TrackAction::Forget {
            url: url.to_string(),
        }
    }

    #[test]
    fn the_current_track_is_downloaded_once() {
        // One play reaches the app three times (current source, LOAD_AUDIO_SOURCE, PLAY). A
        // second download of the same URL would write the same part file.
        let mut planner = TrackPlanner::default();
        assert_eq!(
            planner.on_room_message(&queue(&["a", "b"], Some("a"))),
            [download("a")]
        );
        assert_eq!(planner.on_room_message(&load("a")), []);
        assert_eq!(planner.on_room_message(&scheduled("PLAY", "a")), []);
        assert_eq!(planner.on_room_message(&scheduled("PAUSE", "a")), []);
        assert_eq!(
            planner.states(),
            &TrackStates::from([("a".to_string(), TrackState::Loading)])
        );
        // K1: a queue track downloads only once it becomes current.
        assert_eq!(
            planner.on_room_message(&scheduled("PAUSE", "b")),
            [download("b")]
        );
    }

    #[test]
    fn at_most_two_downloads_run_at_once() {
        // Skipping through the queue must not start a download per skip at once.
        let mut planner = TrackPlanner::default();
        assert_eq!(
            planner.on_room_message(&scheduled("PLAY", "a")),
            [download("a")]
        );
        assert_eq!(planner.on_room_message(&load("b")), [download("b")]);
        assert_eq!(planner.on_room_message(&scheduled("PLAY", "c")), []);
        assert_eq!(planner.on_room_message(&scheduled("PLAY", "d")), []);
        // A waiting track shows as loading.
        assert_eq!(planner.states()["d"], TrackState::Loading);
        // The waiting ones start in arrival order as slots free up.
        assert_eq!(planner.on_done("b", Ok(1.0)), [download("c")]);
        assert_eq!(planner.on_done("a", Err("x".to_string())), [download("d")]);
        assert_eq!(planner.on_done("c", Ok(1.0)), []);
    }

    #[test]
    fn a_url_that_leaves_the_queue_is_forgotten() {
        let mut planner = TrackPlanner::default();
        planner.on_room_message(&queue(&["a", "b", "c", "d"], Some("a")));
        planner.on_done("a", Ok(2.0));
        planner.on_room_message(&scheduled("PLAY", "b"));
        planner.on_room_message(&scheduled("PLAY", "c"));
        planner.on_room_message(&scheduled("PLAY", "d"));
        // a is ready, b and c download, d waits. Only d stays in the queue, and it gets a
        // freed slot.
        assert_eq!(
            planner.on_room_message(&queue(&["d"], None)),
            [forget("a"), forget("b"), forget("c"), download("d")]
        );
        assert_eq!(
            planner.states(),
            &TrackStates::from([("d".to_string(), TrackState::Loading)])
        );
        // The result of a forgotten download changes nothing.
        assert_eq!(planner.on_done("b", Ok(2.0)), []);
        assert!(!planner.states().contains_key("b"));
        // A current source that is not in the queue is not downloaded.
        assert_eq!(planner.on_room_message(&queue(&["d"], Some("gone"))), []);
        // A URL that comes back downloads again.
        assert_eq!(
            planner.on_room_message(&scheduled("PLAY", "a")),
            [download("a")]
        );
    }

    #[test]
    fn results_set_ready_or_error() {
        let mut planner = TrackPlanner::default();
        planner.on_room_message(&scheduled("PLAY", "a"));
        planner.on_room_message(&scheduled("PLAY", "b"));
        planner.on_done("a", Ok(2.0));
        planner.on_done("b", Err("Download failed: 404 Not Found".to_string()));
        assert_eq!(planner.states()["a"], TrackState::Ready { duration_s: 2.0 });
        assert_eq!(
            planner.states()["b"],
            TrackState::Error {
                message: "Download failed: 404 Not Found".to_string()
            }
        );
        // A play does not retry a failed track; only LOAD_AUDIO_SOURCE does.
        assert_eq!(planner.on_room_message(&scheduled("PLAY", "b")), []);
    }

    #[test]
    fn a_load_audio_source_retries_an_error_track() {
        // The web reloads an error source on every LOAD_AUDIO_SOURCE.
        let mut planner = TrackPlanner::default();
        planner.on_room_message(&scheduled("PLAY", "a"));
        planner.on_room_message(&scheduled("PLAY", "b"));
        planner.on_done("a", Err("The connection timed out".to_string()));
        assert!(matches!(planner.states()["a"], TrackState::Error { .. }));
        assert_eq!(planner.on_room_message(&load("a")), [download("a")]);
        assert_eq!(planner.states()["a"], TrackState::Loading);
        // Only one download runs for the URL: a second load message adds nothing.
        assert_eq!(planner.on_room_message(&load("a")), []);
        assert_eq!(planner.on_done("a", Ok(3.0)), []);
        assert_eq!(planner.states()["a"], TrackState::Ready { duration_s: 3.0 });
        // A ready track is not loaded again.
        assert_eq!(planner.on_room_message(&load("a")), []);
    }

    #[test]
    fn a_retry_waits_for_a_free_slot() {
        let mut planner = TrackPlanner::default();
        planner.on_room_message(&scheduled("PLAY", "a"));
        planner.on_room_message(&scheduled("PLAY", "b"));
        planner.on_done("a", Err("x".to_string()));
        planner.on_room_message(&scheduled("PLAY", "c"));
        // b and c run: the retry of a waits.
        assert_eq!(planner.on_room_message(&load("a")), []);
        assert_eq!(planner.states()["a"], TrackState::Loading);
        assert_eq!(planner.on_done("b", Ok(1.0)), [download("a")]);
    }

    #[test]
    fn track_states_serialize_as_the_window_reads_them() {
        // R-pre-1: the window's TrackState type has the tag `state` and the field `durationS`.
        let states = TrackStates::from([
            ("a".to_string(), TrackState::Loading),
            ("b".to_string(), TrackState::Ready { duration_s: 2.0 }),
            (
                "c".to_string(),
                TrackState::Error {
                    message: "Cannot play this track".to_string(),
                },
            ),
        ]);
        assert_eq!(
            serde_json::to_value(&states).unwrap(),
            json!({
                "a": { "state": "loading" },
                "b": { "state": "ready", "durationS": 2.0 },
                "c": { "state": "error", "message": "Cannot play this track" },
            })
        );
    }

    #[test]
    fn each_url_has_its_own_cache_file() {
        // Two URLs with one file would overwrite each other; the extension is the probe's
        // format hint.
        let a = track_file_name("https://r2.example/room-123456/Song%E2%98%86171.MP3?x=1");
        let b = track_file_name("https://r2.example/room-123456/Song%E2%98%86172.MP3");
        assert_ne!(a, b);
        assert!(a.len() == 20 && a.ends_with(".mp3"), "{a}");
        assert!(track_file_name("/audio/a.flac").ends_with(".flac"));
        assert!(track_file_name("https://r2.example/stream").ends_with(".bin"));
        // The FNV-1a test vector for "a".
        assert_eq!(track_file_name("a"), "af63dc4c8601ec8c.bin");
        // An "extension" that could leave the cache dir is none.
        assert!(track_file_name("https://x/a.mp3%2F..%2Fx").ends_with(".bin"));
    }
}
