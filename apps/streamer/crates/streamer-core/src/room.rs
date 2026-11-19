//! Room-level server messages for the app window: which messages the core forwards to it, the
//! cache that replays the room to a (re)loaded window, and which actions the window may send.
//! The messages stay raw `@beatsync/shared` JSON: the window validates them with the shared
//! schemas. Pure: no I/O.

use serde_json::{Value, json};

fn message_type(msg: &Value) -> Option<&str> {
    msg.get("type")?.as_str()
}

/// The server messages the window gets. The others stay in the core (clock sync, liveness) or
/// are of no use to the window (demo counts, errors, unknown types).
pub fn forwarded(msg: &Value) -> bool {
    matches!(
        message_type(msg),
        Some(
            "ROOM_EVENT"
                | "SCHEDULED_ACTION"
                | "SEARCH_RESPONSE"
                | "STREAM_JOB_UPDATE"
                | "LIVE_STREAM_REJECTED"
        )
    )
}

/// The room state from the forwarded messages, kept to replay the room to a (re)loaded
/// window: the latest message of each slot. Transient messages (LOAD_AUDIO_SOURCE,
/// SEARCH_RESPONSE, LIVE_STREAM_REJECTED) and unknown types are not kept.
#[derive(Debug, Default)]
pub struct RoomCache {
    clients: Option<Value>,
    playback_controls: Option<Value>,
    audio_sources: Option<Value>,
    live_stream: Option<Value>,
    chat: Option<Chat>,
    global_volume: Option<Value>,
    /// SPATIAL_CONFIG, or the STOP_SPATIAL_AUDIO that ended it.
    spatial: Option<Value>,
    low_pass: Option<Value>,
    metronome: Option<Value>,
    /// The last PLAY or PAUSE.
    playback: Option<Value>,
    stream_jobs: Option<Value>,
}

/// The chat as the web client's store keeps it.
#[derive(Debug, Default)]
struct Chat {
    messages: Vec<Value>,
    newest_id: u64,
}

impl RoomCache {
    pub fn apply(&mut self, msg: &Value) {
        let slot = match message_type(msg) {
            Some("ROOM_EVENT") => match message_type(&msg["event"]) {
                Some("CLIENT_CHANGE") => &mut self.clients,
                Some("SET_PLAYBACK_CONTROLS") => &mut self.playback_controls,
                Some("SET_AUDIO_SOURCES") => {
                    self.drop_playback_of_deleted_track(&msg["event"]);
                    &mut self.audio_sources
                }
                Some("LIVE_STREAM_STATE") => &mut self.live_stream,
                Some("CHAT_UPDATE") => {
                    self.apply_chat(&msg["event"]);
                    return;
                }
                _ => return,
            },
            Some("SCHEDULED_ACTION") => match message_type(&msg["scheduledAction"]) {
                Some("GLOBAL_VOLUME_CONFIG") => &mut self.global_volume,
                Some("SPATIAL_CONFIG" | "STOP_SPATIAL_AUDIO") => &mut self.spatial,
                Some("LOW_PASS_CONFIG") => &mut self.low_pass,
                Some("METRONOME_CONFIG") => &mut self.metronome,
                Some("PLAY" | "PAUSE") => &mut self.playback,
                _ => return,
            },
            Some("STREAM_JOB_UPDATE") => &mut self.stream_jobs,
            _ => return,
        };
        *slot = Some(msg.clone());
    }

    /// The server deletes the current track without a PAUSE: a replayed PLAY of it would show a
    /// deleted track as playing.
    fn drop_playback_of_deleted_track(&mut self, event: &Value) {
        let Some(sources) = event["sources"].as_array() else {
            return;
        };
        let playing = self
            .playback
            .as_ref()
            .and_then(|p| p["scheduledAction"]["audioSource"].as_str());
        if playing.is_some_and(|url| !sources.iter().any(|s| s["url"].as_str() == Some(url))) {
            self.playback = None;
        }
    }

    /// The web chat store's rule (`store/chat.tsx`): a full sync replaces the messages; an
    /// update appends the messages newer than the newest one kept.
    fn apply_chat(&mut self, event: &Value) {
        let (Some(messages), Some(is_full_sync), Some(newest_id)) = (
            event["messages"].as_array(),
            event["isFullSync"].as_bool(),
            event["newestId"].as_u64(),
        ) else {
            return;
        };
        let chat = self.chat.get_or_insert_with(Chat::default);
        if is_full_sync {
            chat.messages = messages.clone();
            chat.newest_id = newest_id;
        } else {
            let kept_id = chat.newest_id;
            let newer = messages
                .iter()
                .filter(|m| m["id"].as_u64().is_some_and(|id| id > kept_id));
            chat.messages.extend(newer.cloned());
            chat.newest_id = kept_id.max(newest_id);
        }
    }

    /// The room as messages, one per slot, in this order: CLIENT_CHANGE,
    /// SET_PLAYBACK_CONTROLS, SET_AUDIO_SOURCES, LIVE_STREAM_STATE, the chat (one full-sync
    /// CHAT_UPDATE), GLOBAL_VOLUME_CONFIG, spatial, LOW_PASS_CONFIG, METRONOME_CONFIG, PLAY or
    /// PAUSE, STREAM_JOB_UPDATE.
    pub fn snapshot(&self) -> Vec<Value> {
        let before_chat = [
            &self.clients,
            &self.playback_controls,
            &self.audio_sources,
            &self.live_stream,
        ];
        let chat = self.chat.as_ref().map(|chat| {
            json!({
                "type": "ROOM_EVENT",
                "event": {
                    "type": "CHAT_UPDATE",
                    "messages": chat.messages,
                    "isFullSync": true,
                    "newestId": chat.newest_id,
                },
            })
        });
        let after_chat = [
            &self.global_volume,
            &self.spatial,
            &self.low_pass,
            &self.metronome,
            &self.playback,
            &self.stream_jobs,
        ];
        before_chat
            .into_iter()
            .flatten()
            .cloned()
            .chain(chat)
            .chain(after_chat.into_iter().flatten().cloned())
            .collect()
    }
}

/// The room actions the window may send. The core sends the others itself: the clock sync,
/// the liveness replies, SYNC and this PC's stream. MOVE_CLIENT stays refused: the web never
/// sends it and the server lets anyone move anyone.
pub fn check_action(action: &Value, streaming: bool) -> Result<(), String> {
    match message_type(action) {
        Some(
            "SEND_CHAT_MESSAGE"
            | "SET_ADMIN"
            | "SET_PLAYBACK_CONTROLS"
            | "PLAY"
            | "PAUSE"
            | "REORDER_AUDIO_SOURCES"
            | "DELETE_AUDIO_SOURCES"
            | "LOAD_DEFAULT_TRACKS"
            | "SEARCH_MUSIC"
            | "STREAM_MUSIC"
            | "SET_GLOBAL_VOLUME"
            | "SET_LOW_PASS_FREQ"
            | "SET_METRONOME"
            | "START_SPATIAL_AUDIO"
            | "STOP_SPATIAL_AUDIO"
            | "SET_LISTENING_SOURCE"
            | "REORDER_CLIENT",
        ) => Ok(()),
        // The window stops another user's stream. This PC's own stream ends through its own
        // stop, which also restores the routing.
        Some("STOP_LIVE_STREAM") if streaming => {
            Err("Use Stop stream to end this PC's stream".to_string())
        }
        Some("STOP_LIVE_STREAM") => Ok(()),
        Some(other) => Err(format!("Action not allowed: {other}")),
        None => Err("Action not allowed: no type".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room_event(event: Value) -> Value {
        json!({ "type": "ROOM_EVENT", "event": event })
    }

    fn scheduled(action: Value) -> Value {
        json!({
            "type": "SCHEDULED_ACTION",
            "serverTimeToExecute": 1_700_000_010_000_u64,
            "scheduledAction": action,
        })
    }

    fn client(id: &str) -> Value {
        json!({ "username": id, "clientId": id, "isAdmin": false, "joinedAt": 1 })
    }

    fn chat(ids: &[u64], is_full_sync: bool, newest_id: u64) -> Value {
        let messages: Vec<Value> = ids
            .iter()
            .map(|&id| {
                json!({
                    "id": id,
                    "clientId": "a",
                    "username": "a",
                    "text": format!("message {id}"),
                    "timestamp": 1_700_000_000_000_u64 + id,
                })
            })
            .collect();
        room_event(json!({
            "type": "CHAT_UPDATE",
            "messages": messages,
            "isFullSync": is_full_sync,
            "newestId": newest_id,
        }))
    }

    fn chat_ids(snapshot_message: &Value) -> Vec<u64> {
        snapshot_message["event"]["messages"]
            .as_array()
            .expect("a message list")
            .iter()
            .map(|m| m["id"].as_u64().expect("an id"))
            .collect()
    }

    #[test]
    fn snapshot_has_the_latest_state_per_slot() {
        // A reloaded window rebuilds the room from the snapshot alone: an old message in a
        // slot shows a stale room, a wrong order lets a later slot see state it depends on
        // missing.
        let clients_a = room_event(json!({ "type": "CLIENT_CHANGE", "clients": [client("a")] }));
        let clients_b = room_event(json!({
            "type": "CLIENT_CHANGE",
            "clients": [client("a"), client("b")],
        }));
        let controls =
            room_event(json!({ "type": "SET_PLAYBACK_CONTROLS", "permissions": "EVERYONE" }));
        let sources = room_event(json!({
            "type": "SET_AUDIO_SOURCES",
            "sources": [{ "url": "https://cdn.example.com/a.mp3" }],
        }));
        let stream = room_event(json!({ "type": "LIVE_STREAM_STATE", "stream": null }));
        let chat_sync = chat(&[1], true, 1);
        let volume =
            scheduled(json!({ "type": "GLOBAL_VOLUME_CONFIG", "volume": 0.8, "rampTime": 0.1 }));
        let spatial = scheduled(json!({
            "type": "SPATIAL_CONFIG",
            "gains": { "a": { "gain": 0.5, "rampTime": 0.1 } },
            "listeningSource": { "x": 50, "y": 50 },
        }));
        let stop_spatial = scheduled(json!({ "type": "STOP_SPATIAL_AUDIO" }));
        let low_pass =
            scheduled(json!({ "type": "LOW_PASS_CONFIG", "freq": 400, "rampTime": 0.1 }));
        let metronome = scheduled(json!({ "type": "METRONOME_CONFIG", "enabled": true }));
        let play = scheduled(json!({
            "type": "PLAY",
            "trackTimeSeconds": 0,
            "audioSource": "https://cdn.example.com/a.mp3",
        }));
        let pause = scheduled(json!({
            "type": "PAUSE",
            "trackTimeSeconds": 3.5,
            "audioSource": "https://cdn.example.com/a.mp3",
        }));
        let jobs = json!({ "type": "STREAM_JOB_UPDATE", "activeJobCount": 2 });

        let mut cache = RoomCache::default();
        for message in [
            &jobs,
            &play,
            &metronome,
            &spatial,
            &low_pass,
            &clients_a,
            &volume,
            &chat_sync,
            &stream,
            &sources,
            &pause,
            &controls,
            &clients_b,
            &stop_spatial,
        ] {
            assert!(forwarded(message), "{message}");
            cache.apply(message);
        }
        assert_eq!(
            cache.snapshot(),
            vec![
                clients_b,
                controls,
                sources,
                stream,
                chat_sync,
                volume,
                stop_spatial,
                low_pass,
                metronome,
                pause,
                jobs,
            ]
        );
    }

    #[test]
    fn a_deleted_track_drops_the_cached_playback() {
        let play = scheduled(json!({ "type": "PLAY", "trackTimeSeconds": 1, "audioSource": "a" }));
        let sources = |urls: &[&str]| {
            let sources: Vec<Value> = urls.iter().map(|url| json!({ "url": url })).collect();
            room_event(json!({ "type": "SET_AUDIO_SOURCES", "sources": sources }))
        };
        let mut cache = RoomCache::default();
        cache.apply(&play);
        cache.apply(&sources(&["a", "b"]));
        assert_eq!(cache.snapshot().len(), 2);
        cache.apply(&sources(&["b"]));
        assert_eq!(cache.snapshot(), vec![sources(&["b"])]);
    }

    #[test]
    fn chat_full_sync_replaces_and_updates_append_newer_ids() {
        // The web chat store's merge rule: an update may repeat messages the full sync had.
        let mut cache = RoomCache::default();
        cache.apply(&chat(&[1, 2], true, 2));
        cache.apply(&chat(&[2, 3], false, 3));
        let snapshot = cache.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0]["type"], "ROOM_EVENT");
        assert_eq!(snapshot[0]["event"]["type"], "CHAT_UPDATE");
        assert_eq!(chat_ids(&snapshot[0]), [1, 2, 3]);
        assert_eq!(snapshot[0]["event"]["newestId"], 3);
        assert_eq!(snapshot[0]["event"]["isFullSync"], true);
        assert_eq!(snapshot[0]["event"]["messages"][2]["text"], "message 3");

        cache.apply(&chat(&[7], true, 7));
        let snapshot = cache.snapshot();
        assert_eq!(chat_ids(&snapshot[0]), [7]);
        assert_eq!(snapshot[0]["event"]["newestId"], 7);
    }

    #[test]
    fn transient_messages_are_forwarded_not_cached() {
        // Replayed after a reload, these would load a track again, show old search results
        // or report a rejection again.
        let load = room_event(json!({
            "type": "LOAD_AUDIO_SOURCE",
            "audioSourceToPlay": { "url": "https://cdn.example.com/a.mp3" },
        }));
        let search = json!({
            "type": "SEARCH_RESPONSE",
            "response": { "type": "success", "response": { "data": { "tracks": { "items": [] } } } },
        });
        let rejected = json!({ "type": "LIVE_STREAM_REJECTED", "reason": "NOT_ALLOWED" });
        let mut cache = RoomCache::default();
        for message in [&load, &search, &rejected] {
            assert!(forwarded(message), "{message}");
            cache.apply(message);
        }
        assert!(cache.snapshot().is_empty());
    }

    #[test]
    fn internal_and_unknown_messages_are_not_forwarded() {
        let ntp = json!({
            "type": "NTP_RESPONSE",
            "t0": 1.0,
            "t1": 2.0,
            "t2": 3.0,
            "probeGroupId": 7,
            "probeGroupIndex": 0,
        });
        let clients = room_event(json!({ "type": "CLIENT_CHANGE", "clients": [client("a")] }));
        let mut cache = RoomCache::default();
        cache.apply(&clients);
        let before = cache.snapshot();
        for message in [
            ntp,
            json!({ "type": "LIVENESS_PING" }),
            json!({ "type": "ERROR" }),
            json!({ "type": "DEMO_USER_COUNT", "count": 3 }),
            json!({ "type": "DEMO_AUDIO_READY_COUNT", "count": 1 }),
            json!(42),
            json!({}),
        ] {
            assert!(!forwarded(&message), "{message}");
            cache.apply(&message);
        }
        assert_eq!(cache.snapshot(), before);
    }

    #[test]
    fn check_action_allows_only_window_actions() {
        let chat = json!({ "type": "SEND_CHAT_MESSAGE", "text": "hi" });
        let admin = json!({ "type": "SET_ADMIN", "clientId": "b", "isAdmin": true });
        let controls = json!({ "type": "SET_PLAYBACK_CONTROLS", "permissions": "ADMIN_ONLY" });
        for action in [&chat, &admin, &controls] {
            assert_eq!(check_action(action, false), Ok(()), "{action}");
            assert_eq!(check_action(action, true), Ok(()), "{action}");
        }
        // The window stops another user's stream; this PC's own stream has its own stop,
        // which also restores the routing.
        let stop = json!({ "type": "STOP_LIVE_STREAM" });
        assert_eq!(check_action(&stop, false), Ok(()));
        assert_eq!(
            check_action(&stop, true),
            Err("Use Stop stream to end this PC's stream".to_string())
        );
        // The core owns the clock sync, the liveness and this PC's stream.
        for kind in [
            "NTP_REQUEST",
            "START_LIVE_STREAM",
            "AUDIO_SOURCE_LOADED",
            "SYNC",
        ] {
            assert_eq!(
                check_action(&json!({ "type": kind }), false),
                Err(format!("Action not allowed: {kind}"))
            );
        }
        assert!(check_action(&json!({ "text": "hi" }), false).is_err());
    }

    #[test]
    fn check_action_allows_room_control() {
        for kind in [
            "PLAY",
            "PAUSE",
            "REORDER_AUDIO_SOURCES",
            "DELETE_AUDIO_SOURCES",
            "LOAD_DEFAULT_TRACKS",
            "SEARCH_MUSIC",
            "STREAM_MUSIC",
            "SET_GLOBAL_VOLUME",
            "SET_LOW_PASS_FREQ",
            "SET_METRONOME",
            "START_SPATIAL_AUDIO",
            "STOP_SPATIAL_AUDIO",
            "SET_LISTENING_SOURCE",
            "REORDER_CLIENT",
        ] {
            let action = json!({ "type": kind });
            assert_eq!(check_action(&action, false), Ok(()), "{kind}");
            assert_eq!(check_action(&action, true), Ok(()), "{kind}");
        }
        // The web never sends MOVE_CLIENT, and the server lets anyone move anyone.
        for kind in [
            "MOVE_CLIENT",
            "SEND_IP",
            "AUDIO_SOURCE_LOADED",
            "LIVENESS_PONG",
            "UPDATE_LIVE_STREAM",
        ] {
            assert_eq!(
                check_action(&json!({ "type": kind }), false),
                Err(format!("Action not allowed: {kind}"))
            );
        }
    }
}
