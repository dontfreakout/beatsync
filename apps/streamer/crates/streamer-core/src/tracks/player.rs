//! The rules of the queue player: which track plays, from which position, when it stops and
//! when the next track starts. Pure: no I/O, no clock, no threads. The loader feeds it the
//! room messages, the load results and a periodic tick with the server time, and turns the
//! actions into audio commands and outgoing messages.

use std::collections::HashMap;

use serde_json::Value;

use super::non_empty;

/// Shuffle draws again while it hits the current track, at most this often; then the next
/// track in queue order plays.
const MAX_SHUFFLE_DRAWS: usize = 32;
/// A `PLAY` of the active track whose position 0 falls within this many ms of the active
/// one's is on the same timeline.
const SAME_TIMELINE_MS: f64 = 1.0;

/// What the owner of the player must do.
#[derive(Debug, Clone, PartialEq)]
pub enum PlayerAction {
    /// Start the track at `position_s` at the server time `start_server_ms`. A start time in
    /// the past means the track plays from the position the room has reached. It replaces
    /// the current track and any pending stop at once.
    Play {
        url: String,
        start_server_ms: f64,
        position_s: f64,
    },
    /// Stop the track at the server time.
    StopAt { server_ms: f64 },
    /// Stop the track now.
    StopNow,
    /// Send a `PLAY` for the track at position 0.
    SendPlay { url: String },
}

/// What the player does with the audio.
#[derive(Debug, Clone, PartialEq)]
enum Phase {
    /// Nothing plays. A natural end never follows from this phase.
    Idle,
    /// A track plays from `position_s` at the server time `start_ms`, or will play from then,
    /// until the server time `end_ms`.
    Playing {
        url: String,
        start_ms: f64,
        position_s: f64,
        end_ms: f64,
    },
    /// A `PLAY` waits for the track to be decoded. The anchor is that of the `PLAY`.
    Pending {
        url: String,
        start_ms: f64,
        position_s: f64,
    },
}

/// Applies the rules of the web player to the room messages: a scheduled `PLAY` starts a
/// track, a scheduled `PAUSE` stops it, and at the natural end of a track a client that may
/// change the room asks for the next one.
#[derive(Debug)]
pub struct QueuePlayer {
    own_client_id: String,
    /// The queue order, by URL.
    queue: Vec<String>,
    /// The track the room selected (`currentAudioSource`, a load request or a `PLAY`).
    selected: Option<String>,
    /// The duration in seconds of each track that is decoded and playable now.
    ready: HashMap<String, f64>,
    phase: Phase,
    everyone_may_control: bool,
    is_admin: bool,
    live_stream: bool,
}

impl QueuePlayer {
    pub fn new(own_client_id: String) -> Self {
        Self {
            own_client_id,
            queue: Vec::new(),
            selected: None,
            ready: HashMap::new(),
            phase: Phase::Idle,
            everyone_may_control: false,
            is_admin: false,
            live_stream: false,
        }
    }

    /// Reads one raw room message. Messages of other kinds change nothing.
    pub fn on_room_message(&mut self, msg: &Value) -> Vec<PlayerAction> {
        match msg["type"].as_str() {
            Some("ROOM_EVENT") => self.on_room_event(&msg["event"]),
            Some("SCHEDULED_ACTION") => self.on_scheduled_action(msg),
            _ => Vec::new(),
        }
    }

    /// The track is decoded and playable now. A `PLAY` that waited for it starts at its
    /// original anchor, so the feeder skips to the position the room has reached; one whose
    /// end the room has passed at `server_now_ms` plays nothing and asks for no next track
    /// (the web aborts a `PLAY` past the end). A track that is not waited for is only
    /// recorded.
    pub fn on_track_ready(
        &mut self,
        url: &str,
        duration_s: f64,
        server_now_ms: f64,
    ) -> Vec<PlayerAction> {
        self.ready.insert(url.to_string(), duration_s);
        match &self.phase {
            Phase::Pending {
                url: pending,
                start_ms,
                position_s,
            } if pending == url => {
                let (start_ms, position_s) = (*start_ms, *position_s);
                let end_ms = start_ms + (duration_s - position_s) * 1000.0;
                if position_s >= duration_s || end_ms <= server_now_ms {
                    // The pending start already stopped the audio.
                    self.phase = Phase::Idle;
                    return Vec::new();
                }
                self.start(url, start_ms, position_s, duration_s)
            }
            _ => Vec::new(),
        }
    }

    /// The track cannot be played. A `PLAY` that waited for it plays nothing.
    pub fn on_track_failed(&mut self, url: &str) {
        self.ready.remove(url);
        if matches!(&self.phase, Phase::Pending { url: pending, .. } if pending == url) {
            self.phase = Phase::Idle;
        }
    }

    /// The decoded file of the track is gone. A later `PLAY` for it waits for a new decode.
    pub fn on_track_unready(&mut self, url: &str) {
        self.ready.remove(url);
    }

    /// The owner could not start the `Play` of the track: its decoded file went first. The
    /// track waits for its next decode and then starts at the same anchor, like a `PLAY` for
    /// a track that is not decoded.
    pub fn on_play_unavailable(&mut self, url: &str) {
        self.ready.remove(url);
        if let Phase::Playing {
            url: playing,
            start_ms,
            position_s,
            ..
        } = &self.phase
            && playing == url
        {
            self.phase = Phase::Pending {
                url: url.to_string(),
                start_ms: *start_ms,
                position_s: *position_s,
            };
        }
    }

    /// Call it often with the server time. At the end of the playing track it asks for the
    /// next one when this client may change the room, no live stream runs and the queue has
    /// more than one track. With `shuffle` the next track is random but never the same one.
    pub fn tick(
        &mut self,
        server_now_ms: f64,
        shuffle: bool,
        rand: &mut dyn FnMut() -> f64,
    ) -> Vec<PlayerAction> {
        let Phase::Playing { url, end_ms, .. } = &self.phase else {
            return Vec::new();
        };
        if server_now_ms < *end_ms {
            return Vec::new();
        }
        let url = url.clone();
        self.phase = Phase::Idle;
        let n = self.queue.len();
        if !self.can_mutate() || self.live_stream || n < 2 {
            return Vec::new();
        }
        // A track that left the queue has no successor.
        let Some(index) = self.queue.iter().position(|u| *u == url) else {
            return Vec::new();
        };
        let in_order = (index + 1) % n;
        let next = if shuffle {
            // A random source stuck on the current track must not hang the owner.
            (0..MAX_SHUFFLE_DRAWS)
                .map(|_| ((rand() * n as f64).floor() as usize).min(n - 1))
                .find(|&candidate| candidate != index)
                .unwrap_or(in_order)
        } else {
            in_order
        };
        vec![PlayerAction::SendPlay {
            url: self.queue[next].clone(),
        }]
    }

    fn can_mutate(&self) -> bool {
        self.everyone_may_control || self.is_admin
    }

    fn on_room_event(&mut self, event: &Value) -> Vec<PlayerAction> {
        match event["type"].as_str() {
            Some("SET_AUDIO_SOURCES") => self.on_audio_sources(event),
            Some("LOAD_AUDIO_SOURCE") => {
                if let Some(url) = non_empty(&event["audioSourceToPlay"]["url"]) {
                    self.selected = Some(url.to_string());
                }
                Vec::new()
            }
            Some("SET_PLAYBACK_CONTROLS") => {
                self.everyone_may_control = event["permissions"].as_str() == Some("EVERYONE");
                Vec::new()
            }
            Some("CLIENT_CHANGE") => {
                self.is_admin = event["clients"].as_array().is_some_and(|clients| {
                    clients.iter().any(|client| {
                        client["clientId"].as_str() == Some(self.own_client_id.as_str())
                            && client["isAdmin"].as_bool() == Some(true)
                    })
                });
                Vec::new()
            }
            Some("LIVE_STREAM_STATE") => {
                self.live_stream = !event["stream"].is_null();
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// The queue order changes. A selected or playing track that left the queue stops and
    /// the selection is cleared.
    fn on_audio_sources(&mut self, event: &Value) -> Vec<PlayerAction> {
        let Some(sources) = event["sources"].as_array() else {
            return Vec::new();
        };
        self.queue = sources
            .iter()
            .filter_map(|source| non_empty(&source["url"]).map(str::to_string))
            .collect();
        let queue = &self.queue;
        self.ready.retain(|url, _| queue.contains(url));
        if let Some(url) =
            non_empty(&event["currentAudioSource"]).filter(|u| queue.contains(&u.to_string()))
        {
            self.selected = Some(url.to_string());
        }
        let gone = |url: &String| !queue.contains(url);
        let selected_gone = self.selected.as_ref().is_some_and(gone);
        let active_gone = match &self.phase {
            Phase::Playing { url, .. } | Phase::Pending { url, .. } => gone(url),
            Phase::Idle => false,
        };
        if selected_gone {
            self.selected = None;
        }
        if selected_gone || active_gone {
            self.phase = Phase::Idle;
            return vec![PlayerAction::StopNow];
        }
        Vec::new()
    }

    /// A `PLAY` or `PAUSE` without its server time is ignored. A `PLAY` of the track that
    /// plays or waits for its decode, on the same timeline, changes nothing: the server
    /// answers a `SYNC` (sent after every connect) with such a `PLAY`, and a restart would
    /// silence this PC for the length of the server's scheduling delay.
    fn on_scheduled_action(&mut self, msg: &Value) -> Vec<PlayerAction> {
        let action = &msg["scheduledAction"];
        let Some(at_ms) = msg["serverTimeToExecute"].as_f64() else {
            return Vec::new();
        };
        match action["type"].as_str() {
            Some("PLAY") => {
                let Some(url) = non_empty(&action["audioSource"]) else {
                    return Vec::new();
                };
                let position_s = action["trackTimeSeconds"].as_f64().unwrap_or(0.0);
                if self.runs_on_timeline(url, at_ms, position_s) {
                    return Vec::new();
                }
                self.selected = Some(url.to_string());
                match self.ready.get(url).copied() {
                    Some(duration_s) => self.start(url, at_ms, position_s, duration_s),
                    None => {
                        self.phase = Phase::Pending {
                            url: url.to_string(),
                            start_ms: at_ms,
                            position_s,
                        };
                        vec![PlayerAction::StopNow]
                    }
                }
            }
            Some("PAUSE") => {
                self.phase = Phase::Idle;
                vec![PlayerAction::StopAt { server_ms: at_ms }]
            }
            _ => Vec::new(),
        }
    }

    /// Whether the track plays or waits for its decode with position 0 at the same server time
    /// (within [`SAME_TIMELINE_MS`]) as a `PLAY` of it from `position_s` at `start_ms`.
    fn runs_on_timeline(&self, url: &str, start_ms: f64, position_s: f64) -> bool {
        let (Phase::Playing {
            url: active,
            start_ms: active_start_ms,
            position_s: active_position_s,
            ..
        }
        | Phase::Pending {
            url: active,
            start_ms: active_start_ms,
            position_s: active_position_s,
        }) = &self.phase
        else {
            return false;
        };
        let origin_ms = start_ms - position_s * 1000.0;
        let active_origin_ms = active_start_ms - active_position_s * 1000.0;
        active == url && (origin_ms - active_origin_ms).abs() <= SAME_TIMELINE_MS
    }

    /// Starts a decoded track. A position at or after its end plays nothing, as on the web.
    fn start(
        &mut self,
        url: &str,
        start_ms: f64,
        position_s: f64,
        duration_s: f64,
    ) -> Vec<PlayerAction> {
        if position_s >= duration_s {
            self.phase = Phase::Idle;
            return vec![PlayerAction::StopNow];
        }
        self.phase = Phase::Playing {
            url: url.to_string(),
            start_ms,
            position_s,
            end_ms: start_ms + (duration_s - position_s) * 1000.0,
        };
        vec![PlayerAction::Play {
            url: url.to_string(),
            start_server_ms: start_ms,
            position_s,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ME: &str = "me";

    fn room_event(event: Value) -> Value {
        json!({ "type": "ROOM_EVENT", "event": event })
    }

    fn queue(urls: &[&str], current: Option<&str>) -> Value {
        let sources: Vec<Value> = urls.iter().map(|url| json!({ "url": url })).collect();
        let mut event = json!({ "type": "SET_AUDIO_SOURCES", "sources": sources });
        if let Some(current) = current {
            event["currentAudioSource"] = json!(current);
        }
        room_event(event)
    }

    fn play_msg(url: &str, at_ms: f64, position_s: f64) -> Value {
        json!({
            "type": "SCHEDULED_ACTION",
            "serverTimeToExecute": at_ms,
            "scheduledAction": { "type": "PLAY", "audioSource": url, "trackTimeSeconds": position_s },
        })
    }

    fn pause_msg(at_ms: f64) -> Value {
        json!({
            "type": "SCHEDULED_ACTION",
            "serverTimeToExecute": at_ms,
            "scheduledAction": { "type": "PAUSE" },
        })
    }

    fn permissions(value: &str) -> Value {
        room_event(json!({ "type": "SET_PLAYBACK_CONTROLS", "permissions": value }))
    }

    fn admin(is_admin: bool) -> Value {
        room_event(json!({
            "type": "CLIENT_CHANGE",
            "clients": [
                { "clientId": "other", "isAdmin": !is_admin },
                { "clientId": ME, "isAdmin": is_admin },
            ],
        }))
    }

    fn live(active: bool) -> Value {
        let stream = if active {
            json!({ "clientId": "x" })
        } else {
            Value::Null
        };
        room_event(json!({ "type": "LIVE_STREAM_STATE", "stream": stream }))
    }

    fn play(url: &str, start_server_ms: f64, position_s: f64) -> PlayerAction {
        PlayerAction::Play {
            url: url.to_string(),
            start_server_ms,
            position_s,
        }
    }

    fn send_play(url: &str) -> PlayerAction {
        PlayerAction::SendPlay {
            url: url.to_string(),
        }
    }

    /// A player with a queue `a`, `b`, `c`, all decoded (10 s each), that may mutate.
    fn ready_player() -> QueuePlayer {
        let mut player = QueuePlayer::new(ME.to_string());
        player.on_room_message(&queue(&["a", "b", "c"], None));
        player.on_room_message(&permissions("EVERYONE"));
        for url in ["a", "b", "c"] {
            player.on_track_ready(url, 10.0, 0.0);
        }
        player
    }

    fn tick(player: &mut QueuePlayer, now: f64) -> Vec<PlayerAction> {
        player.tick(now, false, &mut || 0.0)
    }

    #[test]
    fn play_starts_at_the_target_position() {
        let mut player = ready_player();
        let actions = player.on_room_message(&play_msg("a", 5000.0, 2.0));
        assert_eq!(actions, vec![play("a", 5000.0, 2.0)]);
    }

    #[test]
    fn pause_stops_at_its_time() {
        let mut player = ready_player();
        player.on_room_message(&play_msg("a", 5000.0, 0.0));
        let actions = player.on_room_message(&pause_msg(7000.0));
        assert_eq!(actions, vec![PlayerAction::StopAt { server_ms: 7000.0 }]);
    }

    #[test]
    fn a_seek_is_a_new_play() {
        let mut player = ready_player();
        player.on_room_message(&play_msg("a", 5000.0, 0.0));
        let actions = player.on_room_message(&play_msg("a", 8000.0, 6.0));
        assert_eq!(actions, vec![play("a", 8000.0, 6.0)]);
        // The new play ends at 8000 + 4000 ms, not at the end of the first one.
        assert!(tick(&mut player, 12_000.0 - 1.0).is_empty());
        assert_eq!(tick(&mut player, 12_000.0), vec![send_play("b")]);
    }

    #[test]
    fn a_play_on_the_same_timeline_is_ignored() {
        // A reconnect asks for the room's state again: the server answers with a new PLAY of
        // the running track, later and further on but on the same timeline. The PC plays on.
        let mut player = ready_player();
        player.on_room_message(&play_msg("a", 5000.0, 1.0));
        // 5000 − 1000 = 4000 and 6500.5 − 2500 = 4000.5: within 1 ms.
        assert!(
            player
                .on_room_message(&play_msg("a", 6500.5, 2.5))
                .is_empty()
        );
        // The track still ends where the first PLAY says: 5000 + 9000 ms.
        assert!(tick(&mut player, 13_999.0).is_empty());
        assert_eq!(tick(&mut player, 14_000.0), vec![send_play("b")]);

        // The same for a track that waits for its decode: it starts at its first anchor.
        let mut player = QueuePlayer::new(ME.to_string());
        player.on_room_message(&queue(&["a", "b"], None));
        assert_eq!(
            player.on_room_message(&play_msg("a", 5000.0, 1.0)),
            vec![PlayerAction::StopNow]
        );
        assert!(
            player
                .on_room_message(&play_msg("a", 7000.0, 2.9995))
                .is_empty()
        );
        assert_eq!(
            player.on_track_ready("a", 10.0, 6000.0),
            vec![play("a", 5000.0, 1.0)]
        );
    }

    #[test]
    fn a_play_on_another_timeline_still_restarts() {
        let mut player = ready_player();
        player.on_room_message(&play_msg("a", 5000.0, 1.0));
        // 1.002 ms off the running timeline: a seek.
        assert_eq!(
            player.on_room_message(&play_msg("a", 6500.0, 2.498_998)),
            vec![play("a", 6500.0, 2.498_998)]
        );
        // The same timeline, but another track.
        assert_eq!(
            player.on_room_message(&play_msg("b", 6500.0, 2.498_998)),
            vec![play("b", 6500.0, 2.498_998)]
        );
        // After a pause the same timeline plays again.
        player.on_room_message(&pause_msg(7000.0));
        assert_eq!(
            player.on_room_message(&play_msg("b", 6500.0, 2.498_998)),
            vec![play("b", 6500.0, 2.498_998)]
        );
    }

    #[test]
    fn a_scheduled_action_without_a_time_is_ignored() {
        let mut player = ready_player();
        player.on_room_message(&play_msg("a", 5000.0, 0.0));
        for action in [
            json!({ "type": "PLAY", "audioSource": "b", "trackTimeSeconds": 0 }),
            json!({ "type": "PAUSE" }),
        ] {
            let msg = json!({ "type": "SCHEDULED_ACTION", "scheduledAction": action });
            assert!(player.on_room_message(&msg).is_empty(), "{action}");
        }
        // Track a plays on to its end.
        assert_eq!(tick(&mut player, 15_000.0), vec![send_play("b")]);
    }

    #[test]
    fn a_track_ready_after_its_start_plays_at_its_original_anchor() {
        let mut player = QueuePlayer::new(ME.to_string());
        player.on_room_message(&queue(&["a", "b"], None));
        let actions = player.on_room_message(&play_msg("a", 5000.0, 1.0));
        assert_eq!(actions, vec![PlayerAction::StopNow]);
        assert_eq!(
            player.on_track_ready("a", 10.0, 6000.0),
            vec![play("a", 5000.0, 1.0)]
        );
        // It ends where the original anchor says: 5000 + 9000 ms.
        player.on_room_message(&permissions("EVERYONE"));
        assert!(tick(&mut player, 13_999.0).is_empty());
        assert_eq!(tick(&mut player, 14_000.0), vec![send_play("b")]);
    }

    #[test]
    fn a_track_ready_after_its_end_plays_nothing() {
        // The room has moved on: the web aborts a PLAY past the end and sends nothing, so a
        // late decode must neither play the old track nor ask for the next one.
        let mut player = QueuePlayer::new(ME.to_string());
        player.on_room_message(&queue(&["a", "b"], None));
        player.on_room_message(&permissions("EVERYONE"));
        player.on_room_message(&play_msg("a", 5000.0, 1.0));
        // The anchored end is 5000 + 9000 ms.
        assert!(player.on_track_ready("a", 10.0, 14_000.0).is_empty());
        assert!(tick(&mut player, 100_000.0).is_empty());
    }

    #[test]
    fn a_play_whose_file_is_gone_waits_for_its_decode() {
        // The decoded file went before the feeder got it: the track starts at the same anchor
        // once it is decoded again.
        let mut player = ready_player();
        assert_eq!(
            player.on_room_message(&play_msg("a", 5000.0, 1.0)),
            vec![play("a", 5000.0, 1.0)]
        );
        player.on_play_unavailable("a");
        assert!(tick(&mut player, 14_000.0).is_empty(), "nothing plays");
        assert_eq!(
            player.on_track_ready("a", 10.0, 6000.0),
            vec![play("a", 5000.0, 1.0)]
        );
        assert_eq!(tick(&mut player, 14_000.0), vec![send_play("b")]);
    }

    #[test]
    fn a_failed_pending_track_stays_silent() {
        let mut player = QueuePlayer::new(ME.to_string());
        player.on_room_message(&queue(&["a", "b"], None));
        player.on_room_message(&permissions("EVERYONE"));
        player.on_room_message(&play_msg("a", 5000.0, 0.0));
        player.on_track_failed("a");
        assert!(player.on_track_ready("a", 10.0, 6000.0).is_empty());
        assert!(tick(&mut player, 100_000.0).is_empty());
    }

    #[test]
    fn a_play_past_the_end_plays_nothing() {
        let mut player = ready_player();
        let actions = player.on_room_message(&play_msg("a", 5000.0, 10.0));
        assert_eq!(actions, vec![PlayerAction::StopNow]);
        assert!(tick(&mut player, 100_000.0).is_empty());

        // The same rule for a pending track once it is decoded.
        let mut player = QueuePlayer::new(ME.to_string());
        player.on_room_message(&queue(&["a", "b"], None));
        player.on_room_message(&permissions("EVERYONE"));
        player.on_room_message(&play_msg("a", 5000.0, 12.0));
        assert!(player.on_track_ready("a", 10.0, 6000.0).is_empty());
        assert!(tick(&mut player, 100_000.0).is_empty());
    }

    #[test]
    fn an_evicted_track_waits_for_its_decode() {
        let mut player = ready_player();
        player.on_track_unready("a");
        let actions = player.on_room_message(&play_msg("a", 5000.0, 1.0));
        assert_eq!(actions, vec![PlayerAction::StopNow]);
        assert_eq!(
            player.on_track_ready("a", 10.0, 6000.0),
            vec![play("a", 5000.0, 1.0)]
        );
    }

    #[test]
    fn natural_end_sends_the_next_play_only_when_can_mutate() {
        let mut player = ready_player();
        player.on_room_message(&play_msg("a", 0.0, 0.0));
        assert!(tick(&mut player, 9_999.0).is_empty());
        assert_eq!(tick(&mut player, 10_000.0), vec![send_play("b")]);
        // The track is idle now: no second send.
        assert!(tick(&mut player, 10_010.0).is_empty());

        for (permission, is_admin, expected) in [
            ("ADMIN_ONLY", false, false),
            ("ADMIN_ONLY", true, true),
            ("EVERYONE", false, true),
        ] {
            let mut player = ready_player();
            player.on_room_message(&permissions(permission));
            player.on_room_message(&admin(is_admin));
            player.on_room_message(&play_msg("a", 0.0, 0.0));
            assert_eq!(!tick(&mut player, 10_000.0).is_empty(), expected);
        }
    }

    #[test]
    fn the_queue_wraps_at_the_end() {
        let mut player = ready_player();
        player.on_room_message(&play_msg("c", 0.0, 0.0));
        assert_eq!(tick(&mut player, 10_000.0), vec![send_play("a")]);
    }

    #[test]
    fn a_one_track_queue_stops_at_the_end() {
        let mut player = ready_player();
        player.on_room_message(&queue(&["a"], None));
        player.on_room_message(&play_msg("a", 0.0, 0.0));
        assert!(tick(&mut player, 10_000.0).is_empty());
    }

    #[test]
    fn shuffle_never_repeats_the_current_track() {
        let mut player = ready_player();
        player.on_room_message(&play_msg("b", 0.0, 0.0));
        // 0.4 * 3 is index 1, the current track; then 0.7 * 3 is index 2.
        let mut values = [0.4, 0.7].into_iter();
        let actions = player.tick(10_000.0, true, &mut || values.next().unwrap());
        assert_eq!(actions, vec![send_play("c")]);
    }

    #[test]
    fn shuffle_with_a_stuck_random_source_takes_the_next_track() {
        // A random source that keeps returning the current track must not hang the loader.
        let mut player = ready_player();
        player.on_room_message(&play_msg("b", 0.0, 0.0));
        let actions = player.tick(10_000.0, true, &mut || 0.4);
        assert_eq!(actions, vec![send_play("c")]);
    }

    #[test]
    fn no_autoplay_during_a_live_stream() {
        let mut player = ready_player();
        player.on_room_message(&live(true));
        player.on_room_message(&play_msg("a", 0.0, 0.0));
        assert!(tick(&mut player, 10_000.0).is_empty());

        let mut player = ready_player();
        player.on_room_message(&live(true));
        player.on_room_message(&live(false));
        player.on_room_message(&play_msg("a", 0.0, 0.0));
        assert_eq!(tick(&mut player, 10_000.0), vec![send_play("b")]);
    }

    #[test]
    fn no_autoplay_after_a_pause() {
        let mut player = ready_player();
        player.on_room_message(&play_msg("a", 0.0, 0.0));
        player.on_room_message(&pause_msg(4000.0));
        assert!(tick(&mut player, 10_000.0).is_empty());
        // A new play clears that.
        player.on_room_message(&play_msg("a", 20_000.0, 0.0));
        assert_eq!(tick(&mut player, 30_000.0), vec![send_play("b")]);
    }

    #[test]
    fn deleting_the_playing_track_stops_it() {
        let mut player = ready_player();
        player.on_room_message(&play_msg("a", 0.0, 0.0));
        let actions = player.on_room_message(&queue(&["b", "c"], None));
        assert_eq!(actions, vec![PlayerAction::StopNow]);
        assert!(tick(&mut player, 10_000.0).is_empty());
        // A change that keeps the playing track does nothing.
        player.on_room_message(&play_msg("b", 20_000.0, 0.0));
        assert!(
            player
                .on_room_message(&queue(&["b", "c"], Some("b")))
                .is_empty()
        );
    }
}
