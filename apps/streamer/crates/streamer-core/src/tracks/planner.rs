//! Load states of the queue tracks, and the planner that decides which track to download and
//! decode (spec C8, call K1), which decoded files to keep, and when to acknowledge a load
//! request. Pure: no I/O, no clock. The loader in [`super::loader`] does the work and reports
//! back.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use serde::Serialize;
use serde_json::Value;
use url::Url;

use super::non_empty;

/// Loads (a download and its decode, or a decode alone) that run at once; more wait, the
/// newest current track first.
pub const MAX_DOWNLOADS: usize = 2;
/// Decoded files that stay on disk. Beyond that, the least recently used one goes, but never
/// that of the selected track.
pub const MAX_DECODED: usize = 3;

/// The load state of one track, as the window reads it: `{"state":"loading"}`,
/// `{"state":"ready","durationS":2.0}`, `{"state":"error","message":"…"}`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
    tag = "state",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum TrackState {
    /// Waiting for a load slot, downloading, or decoding.
    Loading,
    /// Decoded; the duration is that of the decoded audio. A track whose decoded file was
    /// evicted stays ready.
    Ready { duration_s: f64 },
    /// The text the queue shows for the track.
    Error { message: String },
}

/// The tracks that became current, by URL as the server sends it.
pub type TrackStates = BTreeMap<String, TrackState>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackAction {
    /// Download the track, then decode it; report with [`TrackPlanner::on_done`].
    Download { url: String },
    /// Decode the downloaded track again (its decoded file was evicted); report with
    /// [`TrackPlanner::on_done`].
    Decode { url: String },
    /// Send `AUDIO_SOURCE_LOADED` for the URL.
    Ack { url: String },
    /// Delete the decoded file of the track; its download stays.
    Evict { url: String },
    /// The URL left the queue: stop its load and delete its files.
    Forget { url: String },
}

/// Decides from the room messages which tracks to load (K1): a track downloads and decodes
/// once it becomes the current track, and its download stays while its URL is in the queue.
/// Every `LOAD_AUDIO_SOURCE` gets one ack: at once when the track is decoded, else when its
/// load ends, also when the load fails.
#[derive(Debug, Default)]
pub struct TrackPlanner {
    states: TrackStates,
    /// The loads that run, at most [`MAX_DOWNLOADS`].
    running: Vec<String>,
    /// Current tracks that wait for a load slot, the newest current first.
    waiting: VecDeque<String>,
    /// The tracks with a decoded file, the most recently used first.
    decoded: VecDeque<String>,
    /// The last track that became current. Its decoded file is never evicted.
    selected: Option<String>,
    /// The number of load requests per URL that wait for the URL's load to end.
    unacked: HashMap<String, usize>,
}

impl TrackPlanner {
    /// A URL becomes current through `SET_AUDIO_SOURCES.currentAudioSource`,
    /// `LOAD_AUDIO_SOURCE` or a scheduled `PLAY`/`PAUSE`; `SET_AUDIO_SOURCES` forgets every
    /// URL that left the queue. `LOAD_AUDIO_SOURCE` and `PLAY` count as a use of a decoded
    /// track.
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
                            let current = non_empty(&event["currentAudioSource"]);
                            if let Some(url) = current.filter(|url| queue.contains(url)) {
                                self.make_current(url, false);
                            }
                        }
                    }
                    Some("LOAD_AUDIO_SOURCE") => {
                        if let Some(url) = non_empty(&event["audioSourceToPlay"]["url"]) {
                            // The only message that retries a failed track (as the web does).
                            self.make_current(url, true);
                            self.use_decoded(url);
                            self.request_ack(url, &mut actions);
                        }
                    }
                    _ => {}
                }
            }
            Some("SCHEDULED_ACTION") => {
                let action = &msg["scheduledAction"];
                match (action["type"].as_str(), non_empty(&action["audioSource"])) {
                    (Some("PLAY"), Some(url)) => {
                        self.make_current(url, false);
                        self.use_decoded(url);
                    }
                    (Some("PAUSE"), Some(url)) => self.make_current(url, false),
                    _ => {}
                }
            }
            _ => {}
        }
        self.start_waiting(&mut actions);
        actions
    }

    /// The result of a [`TrackAction::Download`] or [`TrackAction::Decode`]: the duration in
    /// seconds of the decoded track, whose file is in place, or the error text. A result for
    /// a URL that was forgotten meanwhile changes nothing.
    pub fn on_done(&mut self, url: &str, result: Result<f64, String>) -> Vec<TrackAction> {
        let Some(index) = self.running.iter().position(|u| u == url) else {
            return Vec::new();
        };
        self.running.remove(index);
        let mut actions = Vec::new();
        self.ack_waiting_requests(url, &mut actions);
        match result {
            Ok(duration_s) => {
                self.states
                    .insert(url.to_string(), TrackState::Ready { duration_s });
                self.decoded.push_front(url.to_string());
                self.evict(&mut actions);
            }
            Err(message) => {
                self.states
                    .insert(url.to_string(), TrackState::Error { message });
            }
        }
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
            self.decoded.retain(|u| *u != url);
            if self.selected.as_ref() == Some(&url) {
                self.selected = None;
            }
            // The load ends here: its requests get their acks as for a failed load.
            self.ack_waiting_requests(&url, actions);
            actions.push(TrackAction::Forget { url });
        }
    }

    /// The URL becomes the selected track and is loaded unless it is decoded or loading
    /// already; a URL that waits for a slot moves to the front. An error stays until the URL
    /// leaves the queue, except that `retry_error` (`LOAD_AUDIO_SOURCE`) loads it again. A
    /// ready track whose decoded file was evicted is decoded again and stays ready meanwhile.
    fn make_current(&mut self, url: &str, retry_error: bool) {
        self.selected = Some(url.to_string());
        if self.running.iter().any(|u| u == url) || self.decoded.iter().any(|u| u == url) {
            return;
        }
        match self.states.get(url) {
            Some(TrackState::Error { .. }) if !retry_error => return,
            Some(TrackState::Ready { .. }) => {}
            _ => {
                self.states.insert(url.to_string(), TrackState::Loading);
            }
        }
        self.waiting.retain(|u| u != url);
        self.waiting.push_front(url.to_string());
    }

    /// Moves a decoded track to the front of the eviction order.
    fn use_decoded(&mut self, url: &str) {
        if let Some(index) = self.decoded.iter().position(|u| u == url)
            && let Some(url) = self.decoded.remove(index)
        {
            self.decoded.push_front(url);
        }
    }

    /// A load request for a decoded track is acked at once; any other waits for the load
    /// that [`Self::make_current`] started or queued.
    fn request_ack(&mut self, url: &str, actions: &mut Vec<TrackAction>) {
        if self.decoded.iter().any(|u| u == url) {
            actions.push(TrackAction::Ack {
                url: url.to_string(),
            });
        } else {
            *self.unacked.entry(url.to_string()).or_default() += 1;
        }
    }

    fn ack_waiting_requests(&mut self, url: &str, actions: &mut Vec<TrackAction>) {
        let count = self.unacked.remove(url).unwrap_or(0);
        actions.extend((0..count).map(|_| TrackAction::Ack {
            url: url.to_string(),
        }));
    }

    /// Evicts the least recently used decoded tracks beyond [`MAX_DECODED`], skipping the
    /// selected one.
    fn evict(&mut self, actions: &mut Vec<TrackAction>) {
        while self.decoded.len() > MAX_DECODED {
            let selected = self.selected.as_ref();
            let Some(index) = self.decoded.iter().rposition(|u| Some(u) != selected) else {
                break;
            };
            if let Some(url) = self.decoded.remove(index) {
                actions.push(TrackAction::Evict { url });
            }
        }
    }

    /// Starts waiting tracks while slots are free: a ready track (its decoded file was
    /// evicted) decodes its download again, any other downloads.
    fn start_waiting(&mut self, actions: &mut Vec<TrackAction>) {
        while self.running.len() < MAX_DOWNLOADS
            && let Some(url) = self.waiting.pop_front()
        {
            self.running.push(url.clone());
            let action = match self.states.get(&url) {
                Some(TrackState::Ready { .. }) => TrackAction::Decode { url },
                _ => TrackAction::Download { url },
            };
            actions.push(action);
        }
    }
}

/// The cache file name of a track: 16 hex digits of the 64-bit FNV-1a hash of the URL, so
/// each URL has its own file, and the extension of the URL path (`.bin` without one) as the
/// decoder's format hint. The extension `pcm` becomes `bin` too: that name belongs to the
/// decoded file.
pub fn track_file_name(url: &str) -> String {
    let extension = extension(url)
        .filter(|extension| extension != DECODED_EXTENSION)
        .unwrap_or_else(|| "bin".to_string());
    format!("{:016x}.{extension}", url_hash(url))
}

/// The name of the decoded file of a track, next to its download: the hash of
/// [`track_file_name`] with the extension `pcm`.
pub fn decoded_file_name(url: &str) -> String {
    format!("{:016x}.{DECODED_EXTENSION}", url_hash(url))
}

const DECODED_EXTENSION: &str = "pcm";
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// The 64-bit FNV-1a hash of the URL.
fn url_hash(url: &str) -> u64 {
    url.bytes().fold(FNV_OFFSET_BASIS, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(FNV_PRIME)
    })
}

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

    fn decode(url: &str) -> TrackAction {
        TrackAction::Decode {
            url: url.to_string(),
        }
    }

    fn ack(url: &str) -> TrackAction {
        TrackAction::Ack {
            url: url.to_string(),
        }
    }

    fn evict(url: &str) -> TrackAction {
        TrackAction::Evict {
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
        // The waiting ones start as slots free up, the newest first.
        assert_eq!(planner.on_done("b", Ok(1.0)), [ack("b"), download("d")]);
        assert_eq!(planner.on_done("a", Err("x".to_string())), [download("c")]);
        assert_eq!(planner.on_done("d", Ok(1.0)), []);
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
        assert_eq!(planner.on_done("a", Ok(3.0)), [ack("a"), ack("a")]);
        assert_eq!(planner.states()["a"], TrackState::Ready { duration_s: 3.0 });
        // A ready track is not loaded again.
        assert_eq!(planner.on_room_message(&load("a")), [ack("a")]);
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
    fn a_load_request_for_a_decoded_track_is_acked_at_once() {
        // The server waits up to 3 s for the ack of every app before the room plays.
        let mut planner = TrackPlanner::default();
        planner.on_room_message(&scheduled("PLAY", "a"));
        // A play is no load request: nothing to ack.
        assert_eq!(planner.on_done("a", Ok(2.0)), []);
        assert_eq!(planner.on_room_message(&load("a")), [ack("a")]);
        // One ack per request.
        assert_eq!(planner.on_room_message(&load("a")), [ack("a")]);
    }

    #[test]
    fn a_load_request_during_a_load_is_acked_when_it_ends() {
        let mut planner = TrackPlanner::default();
        for url in ["a", "b", "c"] {
            planner.on_room_message(&scheduled("PLAY", url));
        }
        // a downloads and c waits for a slot: their acks wait for the loads.
        assert_eq!(planner.on_room_message(&load("a")), []);
        assert_eq!(planner.on_room_message(&load("a")), []);
        assert_eq!(planner.on_room_message(&load("c")), []);
        assert_eq!(
            planner.on_done("a", Ok(2.0)),
            [ack("a"), ack("a"), download("c")]
        );
        assert_eq!(planner.on_done("b", Ok(2.0)), []);
        assert_eq!(planner.on_done("c", Ok(2.0)), [ack("c")]);
    }

    #[test]
    fn a_failed_load_still_acks() {
        // The web sends no ack for a failed load, and the room waits the full 3 s for it.
        let mut planner = TrackPlanner::default();
        assert_eq!(planner.on_room_message(&load("a")), [download("a")]);
        assert_eq!(planner.on_done("a", Err("x".to_string())), [ack("a")]);
        // The retry acks when it ends, too.
        assert_eq!(planner.on_room_message(&load("a")), [download("a")]);
        assert_eq!(planner.on_done("a", Err("x".to_string())), [ack("a")]);
        // So does a load that stops because its track left the queue.
        planner.on_room_message(&queue(&["a", "b"], None));
        assert_eq!(planner.on_room_message(&load("b")), [download("b")]);
        assert_eq!(
            planner.on_room_message(&queue(&["a"], None)),
            [ack("b"), forget("b")]
        );
    }

    #[test]
    fn the_lru_keeps_three_decoded_tracks_and_never_the_selected_one() {
        // A decoded file takes about 690 MB per hour of audio. The selected track is the one
        // the room plays or is about to play.
        let mut planner = TrackPlanner::default();
        for url in ["a", "b", "c"] {
            planner.on_room_message(&scheduled("PLAY", url));
            assert_eq!(planner.on_done(url, Ok(1.0)), []);
        }
        // A load request uses a, so b is the least recently used.
        assert_eq!(planner.on_room_message(&load("a")), [ack("a")]);
        planner.on_room_message(&scheduled("PLAY", "d"));
        assert_eq!(planner.on_done("d", Ok(1.0)), [evict("b")]);
        // A pause selects c, now the least recently used: a goes instead.
        planner.on_room_message(&scheduled("PLAY", "e"));
        planner.on_room_message(&scheduled("PAUSE", "c"));
        assert_eq!(planner.on_done("e", Ok(1.0)), [evict("a")]);
        // An evicted track stays ready.
        assert_eq!(planner.states()["a"], TrackState::Ready { duration_s: 1.0 });
        assert_eq!(planner.states()["b"], TrackState::Ready { duration_s: 1.0 });
    }

    #[test]
    fn an_evicted_track_is_decoded_again_before_its_ack() {
        let mut planner = TrackPlanner::default();
        for url in ["a", "b", "c", "d"] {
            planner.on_room_message(&scheduled("PLAY", url));
            planner.on_done(url, Ok(1.0));
        }
        // d evicted a. Its download stays, so a load request decodes it again.
        assert_eq!(planner.on_room_message(&load("a")), [decode("a")]);
        assert_eq!(planner.states()["a"], TrackState::Ready { duration_s: 1.0 });
        assert_eq!(planner.on_done("a", Ok(1.0)), [ack("a"), evict("b")]);
        // A play needs the track too. The decode takes a load slot, so it waits behind two
        // downloads.
        planner.on_room_message(&scheduled("PLAY", "x"));
        planner.on_room_message(&scheduled("PLAY", "y"));
        assert_eq!(planner.on_room_message(&scheduled("PLAY", "b")), []);
        assert_eq!(planner.on_done("x", Ok(1.0)), [evict("c"), decode("b")]);
    }

    #[test]
    fn the_newest_current_track_starts_first() {
        // Skipping through the queue: the track the room is about to play must not wait
        // behind the ones skipped past, or the PC misses the server's 3 s load wait.
        let mut planner = TrackPlanner::default();
        for url in ["a", "b", "c", "d", "e"] {
            planner.on_room_message(&scheduled("PLAY", url));
        }
        assert_eq!(planner.on_done("a", Ok(1.0)), [download("e")]);
        // A waiting track named again moves to the front.
        assert_eq!(planner.on_room_message(&load("c")), []);
        assert_eq!(planner.on_done("b", Ok(1.0)), [download("c")]);
        assert_eq!(planner.on_done("e", Ok(1.0)), [download("d")]);
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
        // Two URLs with one file would overwrite each other; the extension is the decoder's
        // format hint.
        let a = track_file_name("https://r2.example/room-123456/Song%E2%98%86171.MP3?x=1");
        let b = track_file_name("https://r2.example/room-123456/Song%E2%98%86172.MP3");
        assert_ne!(a, b);
        assert!(a.len() == 20 && a.ends_with(".mp3"), "{a}");
        assert!(track_file_name("/audio/a.flac").ends_with(".flac"));
        assert!(track_file_name("https://r2.example/stream").ends_with(".bin"));
        // The FNV-1a test vector for "a".
        assert_eq!(track_file_name("a"), "af63dc4c8601ec8c.bin");
        // The decoded file sits next to the download, and a download never takes its name.
        assert_eq!(decoded_file_name("a"), "af63dc4c8601ec8c.pcm");
        assert!(track_file_name("https://x/a.PCM").ends_with(".bin"));
        // An "extension" that could leave the cache dir is none.
        assert!(track_file_name("https://x/a.mp3%2F..%2Fx").ends_with(".bin"));
    }
}
