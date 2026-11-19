//! The room state for the window (web-parity spec §6.2, §6.3): the room messages of the
//! current connection, cached per slot ([`RoomCache`]) and sent to every subscribed window.
//! A window that subscribes (again, e.g. after a reload) gets the cached room first, then the
//! live messages.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde_json::Value;
use tauri::ipc::Channel;

use streamer_core::room::RoomCache;

/// Where the room messages of one window go.
pub trait RoomSink {
    fn send(&self, msg: Value) -> Result<(), String>;
}

impl RoomSink for Channel<Value> {
    fn send(&self, msg: Value) -> Result<(), String> {
        Channel::<Value>::send(self, msg).map_err(|e| e.to_string())
    }
}

/// Sends happen under the lock, so a new subscriber gets the snapshot and then every later
/// message, with no gap and no repeat.
pub struct RoomHub<S> {
    inner: Mutex<Inner<S>>,
}

struct Inner<S> {
    /// Increased for each join and leave; messages of older connections are dropped.
    generation: u64,
    cache: RoomCache,
    /// By webview label.
    sinks: HashMap<String, S>,
}

impl<S: RoomSink> RoomHub<S> {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                generation: 0,
                cache: RoomCache::default(),
                sinks: HashMap::new(),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner<S>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Sends the cached room to `sink`, then registers it for the live messages. A new sink
    /// of the same webview replaces the old one.
    pub fn subscribe(&self, label: String, sink: S) -> Result<(), String> {
        let mut inner = self.lock();
        for msg in inner.cache.snapshot() {
            sink.send(msg)
                .map_err(|e| format!("Cannot send the room: {e}"))?;
        }
        inner.sinks.insert(label, sink);
        Ok(())
    }

    /// Starts a new generation with an empty room and returns it.
    pub fn next_generation(&self) -> u64 {
        let mut inner = self.lock();
        inner.generation += 1;
        inner.cache = RoomCache::default();
        inner.generation
    }

    /// Caches `msg` and sends it to every sink; a sink that fails is dropped. False when
    /// `generation` is no longer the current one (nothing changes then).
    pub fn publish(&self, generation: u64, msg: Value) -> bool {
        let mut inner = self.lock();
        if inner.generation != generation {
            return false;
        }
        inner.cache.apply(&msg);
        inner
            .sinks
            .retain(|label, sink| match sink.send(msg.clone()) {
                Ok(()) => true,
                Err(e) => {
                    eprintln!("streamer-app: dropping the room channel of {label}: {e}");
                    false
                }
            });
        true
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use serde_json::{Value, json};

    use super::*;

    /// Records what it gets; fails every send while `failing` is set.
    #[derive(Clone, Default)]
    struct TestSink {
        got: Arc<Mutex<Vec<Value>>>,
        failing: Arc<AtomicBool>,
    }

    impl TestSink {
        fn got(&self) -> Vec<Value> {
            self.got.lock().unwrap().clone()
        }

        fn set_failing(&self, failing: bool) {
            self.failing.store(failing, Ordering::SeqCst);
        }
    }

    impl RoomSink for TestSink {
        fn send(&self, msg: Value) -> Result<(), String> {
            if self.failing.load(Ordering::SeqCst) {
                return Err("the window is gone".to_string());
            }
            self.got.lock().unwrap().push(msg);
            Ok(())
        }
    }

    fn clients(names: &[&str]) -> Value {
        let clients: Vec<Value> = names
            .iter()
            .map(|name| {
                json!({ "username": name, "clientId": name, "isAdmin": false, "joinedAt": 1 })
            })
            .collect();
        json!({ "type": "ROOM_EVENT", "event": { "type": "CLIENT_CHANGE", "clients": clients } })
    }

    fn chat_update(id: u64) -> Value {
        json!({
            "type": "ROOM_EVENT",
            "event": {
                "type": "CHAT_UPDATE",
                "messages": [{
                    "id": id,
                    "clientId": "a",
                    "username": "a",
                    "text": "hi",
                    "timestamp": 1_700_000_000_000_u64,
                }],
                "isFullSync": false,
                "newestId": id,
            },
        })
    }

    #[test]
    fn subscribe_gets_the_snapshot_then_live_messages() {
        // A reloaded window rebuilds the room from the snapshot and then follows the live
        // messages: a gap or a repeat between the two shows a wrong room.
        let hub = RoomHub::new();
        let generation = hub.next_generation();
        assert!(hub.publish(generation, clients(&["a", "b"])));

        let sink = TestSink::default();
        hub.subscribe("main".to_string(), sink.clone()).unwrap();
        assert_eq!(sink.got(), vec![clients(&["a", "b"])]);

        assert!(hub.publish(generation, chat_update(1)));
        assert_eq!(sink.got(), vec![clients(&["a", "b"]), chat_update(1)]);
    }

    #[test]
    fn a_new_generation_drops_the_old_room() {
        // After a leave (or a new join) the window must not see the users of the old room.
        let hub = RoomHub::new();
        let generation = hub.next_generation();
        assert!(hub.publish(generation, clients(&["a"])));
        hub.next_generation();

        let sink = TestSink::default();
        hub.subscribe("main".to_string(), sink.clone()).unwrap();
        assert_eq!(sink.got(), Vec::<Value>::new());
    }

    #[test]
    fn messages_of_an_old_generation_are_dropped() {
        // The feed of a closed connection can still hold messages of the old room.
        let hub = RoomHub::new();
        let old = hub.next_generation();
        let current = hub.next_generation();
        assert!(hub.publish(current, clients(&["a"])));
        let sink = TestSink::default();
        hub.subscribe("main".to_string(), sink.clone()).unwrap();

        assert!(!hub.publish(old, clients(&["x", "y"])));
        assert_eq!(sink.got(), vec![clients(&["a"])], "not broadcast");

        let later = TestSink::default();
        hub.subscribe("other".to_string(), later.clone()).unwrap();
        assert_eq!(later.got(), vec![clients(&["a"])], "not cached");
    }

    #[test]
    fn a_failing_sink_is_dropped() {
        let hub = RoomHub::new();
        let generation = hub.next_generation();
        let healthy = TestSink::default();
        let broken = TestSink::default();
        hub.subscribe("healthy".to_string(), healthy.clone())
            .unwrap();
        hub.subscribe("broken".to_string(), broken.clone()).unwrap();

        broken.set_failing(true);
        assert!(hub.publish(generation, clients(&["a"])));
        broken.set_failing(false);
        assert!(hub.publish(generation, chat_update(1)));
        assert_eq!(healthy.got(), vec![clients(&["a"]), chat_update(1)]);
        assert_eq!(
            broken.got(),
            Vec::<Value>::new(),
            "dropped at the first failure"
        );

        // A sink that cannot take the snapshot is refused.
        let refused = TestSink::default();
        refused.set_failing(true);
        assert!(
            hub.subscribe("refused".to_string(), refused.clone())
                .is_err()
        );
        refused.set_failing(false);
        assert!(hub.publish(generation, chat_update(2)));
        assert_eq!(refused.got(), Vec::<Value>::new());
    }
}
