//! The per-run virtual sink and the routing of selected app streams to it (spec §9.3).
//! Runs on the PipeWire thread.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Instant;

use ::pipewire as pw;
use pw::metadata::Metadata;
use pw::node::Node;
use pw::properties::{PropertiesBox, properties};

use super::capture::CaptureStream;
use super::thread::Reply;
use super::{RESTORE_RACE, ROUTE_DEBOUNCE};
use crate::audio::apps::{
    MetaValue, RouteAction, StreamNode, app_key, moved_away, routing_actions,
};
use crate::audio::{AudioError, AudioEvent, AudioIo, CapturedChunk, PlayoutShared};

const TARGET_OBJECT: &str = "target.object";
/// WirePlumber 0.4's stream-restore writes this entry (value: a node id) when it restores a
/// stream to an output; WirePlumber 0.5 honours it too.
const TARGET_NODE: &str = "target.node";

/// `node.name` of the `n`-th sink of process `pid`.
pub(super) fn sink_name(pid: u32, n: u32) -> String {
    format!("beatsync-{pid}-{n}")
}

/// Whether `node_name` is a sink of process `pid` (any run).
pub(super) fn is_own_sink(node_name: &str, pid: u32) -> bool {
    node_name.starts_with(&format!("beatsync-{pid}-"))
}

/// Properties of the sink. Unique per process and start, so WirePlumber's stream-restore,
/// which remembers targets by `node.name`, never sends a stream to a later sink. No
/// `object.linger`: PipeWire removes the sink when this connection closes, also on a crash.
pub(super) fn sink_properties(node_name: &str) -> PropertiesBox {
    properties! {
        "factory.name" => "support.null-audio-sink",
        "node.name" => node_name,
        "node.description" => "Beatsync",
        "media.class" => "Audio/Sink",
        "audio.rate" => "48000",
        "audio.channels" => "2",
        "audio.position" => "FL,FR",
        // WirePlumber picks the default output and input by this priority (real devices
        // have 1000 and more), so the sink never becomes a default.
        "priority.session" => "0",
        // Keeps per-run entries out of WirePlumber's stream-properties state file.
        "state.restore-props" => "false",
    }
}

/// The sink as the registry announced it.
pub(super) struct SinkGlobal {
    pub id: u32,
    /// `object.serial`, the value `target.object` takes.
    pub serial: String,
}

/// One start..stop run: the sink, its capture, the selection and the streams moved to the
/// sink. The output is not part of it.
pub(super) struct Session {
    pub sink_name: String,
    /// Dropping the proxy destroys the sink on the server.
    sink: Option<Node>,
    pub sink_proxy_id: u32,
    pub sink_global: Option<SinkGlobal>,
    selected: Vec<String>,
    /// Moved stream id -> its `target.object` metadata entry before the move.
    moved: BTreeMap<u32, Option<MetaValue>>,
    /// Moved streams the user moved away from the sink: left where the user put them while
    /// their app stays selected.
    released: BTreeSet<u32>,
    /// Streams that appeared during this run, with the time (see [`RESTORE_RACE`]).
    added: BTreeMap<u32, Instant>,
    route_at: Option<Instant>,
    /// Answered when the sink shows up in the registry.
    pub pending_start: Option<(Instant, Reply<()>)>,
    failed: bool,
    events: tokio::sync::mpsc::UnboundedSender<AudioEvent>,
    pub shared: Arc<PlayoutShared>,
    /// The capture ring's end, until the capture stream takes it.
    capture_tx: Option<rtrb::Producer<CapturedChunk>>,
    pub capture: Option<CaptureStream>,
}

impl Session {
    pub fn new(
        sink_name: String,
        sink: Node,
        selected: Vec<String>,
        io: AudioIo,
        start_deadline: Instant,
        reply: Reply<()>,
    ) -> Self {
        let sink_proxy_id = pw::proxy::ProxyT::upcast_ref(&sink).id();
        let AudioIo {
            capture_tx,
            shared,
            events,
        } = io;
        Self {
            sink_name,
            sink: Some(sink),
            sink_proxy_id,
            sink_global: None,
            selected,
            moved: BTreeMap::new(),
            released: BTreeSet::new(),
            added: BTreeMap::new(),
            route_at: None,
            pending_start: Some((start_deadline, reply)),
            failed: false,
            events,
            shared,
            capture_tx: Some(capture_tx),
            capture: None,
        }
    }

    /// Routing runs once the sink exists and until a failure.
    pub fn is_routing(&self) -> bool {
        self.sink_global.is_some() && self.pending_start.is_none() && !self.failed
    }

    pub fn route_at(&self) -> Option<Instant> {
        self.route_at
    }

    pub fn select(&mut self, apps: Vec<String>, now: Instant) {
        self.selected = apps;
        self.mark_dirty(now);
    }

    /// A stream appeared.
    pub fn note_added(&mut self, node_id: u32, now: Instant) {
        self.added.insert(node_id, now);
    }

    /// Whether the stream appeared less than [`RESTORE_RACE`] ago.
    fn is_new(&self, node_id: u32, now: Instant) -> bool {
        self.added
            .get(&node_id)
            .is_some_and(|at| now.saturating_duration_since(*at) <= RESTORE_RACE)
    }

    /// Whether routing must run at once: a stream of a selected app is not on the sink, so it
    /// plays undelayed on another output until it is. That is a stream not moved yet, or a
    /// new stream whose move WirePlumber's stream-restore has just overwritten.
    pub fn wants_route_now(
        &self,
        nodes: &[StreamNode],
        targets: &BTreeMap<u32, MetaValue>,
        now: Instant,
    ) -> bool {
        let Some(serial) = self.sink_global.as_ref().map(|s| s.serial.as_str()) else {
            return false;
        };
        let unmoved = || {
            nodes.iter().any(|n| {
                !self.moved.contains_key(&n.id)
                    && !self.released.contains(&n.id)
                    && app_key(n).is_some_and(|k| self.selected.contains(&k))
            })
        };
        let overwritten = || {
            self.moved.keys().any(|id| {
                self.is_new(*id, now) && targets.get(id).is_none_or(|t| t.value != serial)
            })
        };
        self.is_routing() && (unmoved() || overwritten())
    }

    /// Schedules routing after a burst of changes; the first change of a burst sets the time.
    pub fn mark_dirty(&mut self, now: Instant) {
        if self.route_at.is_none() {
            self.route_at = Some(now + ROUTE_DEBOUNCE);
        }
    }

    pub fn take_due(&mut self, now: Instant) -> bool {
        if self.route_at.is_some_and(|at| at <= now) {
            self.route_at = None;
            return true;
        }
        false
    }

    /// A stream vanished; PipeWire drops its metadata entries itself.
    pub fn forget(&mut self, node_id: u32) {
        self.moved.remove(&node_id);
        self.released.remove(&node_id);
        self.added.remove(&node_id);
    }

    /// Stops managing moved streams that the user moved away; their target stays. A new
    /// stream whose entry changed is not released: WirePlumber restored a remembered target
    /// over the move, so the stream is moved again (by the next routing, while its app is
    /// selected) and goes back to that target at the end.
    fn release_moved_away(
        &mut self,
        targets: &BTreeMap<u32, MetaValue>,
        serial: &str,
        now: Instant,
    ) {
        let moved: BTreeSet<u32> = self.moved.keys().copied().collect();
        for node_id in moved_away(&moved, targets, serial) {
            self.moved.remove(&node_id);
            if !self.is_new(node_id, now) {
                self.released.insert(node_id);
            }
        }
    }

    /// Destroys the sink on the server and leaves everything else as it is.
    #[cfg(test)]
    pub fn drop_sink(&mut self) {
        drop(self.sink.take());
    }

    /// Reports a failure to the runtime once and stops routing.
    pub fn fail(&mut self, message: &str) {
        if !self.failed {
            self.failed = true;
            let _ = self.events.send(AudioEvent::Failed(message.to_string()));
        }
    }

    /// Moves streams of selected apps to the sink and restores unselected ones.
    /// `nodes` must not contain the streamer's own streams.
    pub fn route(
        &mut self,
        metadata: &Metadata,
        nodes: &[StreamNode],
        targets: &mut BTreeMap<u32, MetaValue>,
        node_targets: &mut BTreeMap<u32, String>,
        now: Instant,
    ) {
        let Some(serial) = self.sink_global.as_ref().map(|s| s.serial.clone()) else {
            return;
        };
        let sink_node = self.sink_global.as_ref().map(|s| s.id.to_string());
        self.added
            .retain(|_, at| now.saturating_duration_since(*at) <= RESTORE_RACE);
        self.release_moved_away(targets, &serial, now);
        // A released stream is moved again only after its app was unselected and selected
        // again (or as a new stream).
        self.released.retain(|id| {
            nodes
                .iter()
                .any(|n| n.id == *id && app_key(n).is_some_and(|k| self.selected.contains(&k)))
        });
        let routable: Vec<StreamNode> = nodes
            .iter()
            .filter(|n| !self.released.contains(&n.id))
            .cloned()
            .collect();
        let moved: BTreeSet<u32> = self.moved.keys().copied().collect();
        for action in routing_actions(&self.selected, &routable, &moved) {
            match action {
                RouteAction::Move { node_id } => {
                    // An entry that already points at the sink (WirePlumber's stream-restore)
                    // is not a target to go back to.
                    let previous = targets.get(&node_id).filter(|t| t.value != serial).cloned();
                    let to_sink = MetaValue {
                        type_: Some("Spa:Id".to_string()),
                        value: serial.clone(),
                    };
                    write_target(metadata, targets, node_id, Some(to_sink));
                    self.moved.insert(node_id, previous);
                }
                RouteAction::Restore { node_id } => {
                    let previous = self.moved.remove(&node_id).flatten();
                    write_target(metadata, targets, node_id, previous);
                    // A `target.node` entry to the sink would keep the stream there (K1).
                    if node_targets.get(&node_id) == sink_node.as_ref() {
                        clear_target_node(metadata, node_targets, node_id);
                    }
                }
            }
        }
    }

    /// Gives every moved stream that still points at the sink its previous target back (or
    /// deletes the entry), and deletes every other entry that points at the sink. Streams the
    /// user moved away keep the user's target. Also deletes every `target.node` entry that
    /// points at the sink (C4); entries to other nodes stay, so a stream goes back to a
    /// remembered output.
    ///
    /// Accepted WirePlumber 0.4 difference (K2): 0.4 saves the sink as the app's target when
    /// it moves a stream, and clears that when the entry is deleted. So 0.4 forgets the app's
    /// remembered output once the app was streamed. Its streams still go back to their output
    /// at stop (the `target.node` entry to that output stays), but the next start of the app
    /// uses the default output.
    pub fn restore_all(
        &mut self,
        metadata: &Metadata,
        targets: &mut BTreeMap<u32, MetaValue>,
        node_targets: &mut BTreeMap<u32, String>,
        now: Instant,
    ) {
        if let Some(serial) = self.sink_global.as_ref().map(|s| s.serial.clone()) {
            self.release_moved_away(targets, &serial, now);
        }
        self.released.clear();
        for (node_id, previous) in std::mem::take(&mut self.moved) {
            write_target(metadata, targets, node_id, previous);
        }
        if let Some(serial) = self.sink_global.as_ref().map(|s| s.serial.clone()) {
            let to_sink: Vec<u32> = targets
                .iter()
                .filter(|(_, t)| t.value == serial)
                .map(|(id, _)| *id)
                .collect();
            for node_id in to_sink {
                write_target(metadata, targets, node_id, None);
            }
        }
        if let Some(sink_node) = self.sink_global.as_ref().map(|s| s.id.to_string()) {
            let to_sink: Vec<u32> = node_targets
                .iter()
                .filter(|(_, value)| **value == sink_node)
                .map(|(id, _)| *id)
                .collect();
            for node_id in to_sink {
                clear_target_node(metadata, node_targets, node_id);
            }
        }
    }

    /// The capture ring's end for the capture stream; `None` once taken.
    pub fn take_capture_tx(&mut self) -> Option<rtrb::Producer<CapturedChunk>> {
        self.capture_tx.take()
    }

    /// Drops the stream with `id` if it is this session's capture, and says what it was.
    pub fn remove_stream(&mut self, id: u64) -> Option<&'static str> {
        if self.capture.as_ref().is_some_and(|s| s.id == id) {
            drop(self.capture.take());
            Some("Audio capture")
        } else {
            None
        }
    }

    /// Answers a pending start with `result`; false when its caller no longer waits.
    pub fn answer_start(&mut self, result: Result<(), AudioError>) -> bool {
        self.pending_start
            .take()
            .is_some_and(|(_, reply)| reply.send(result).is_ok())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.answer_start(Err(AudioError::Backend(
            "Audio routing stopped before the sink was ready".to_string(),
        )));
        // The capture goes first, so it never sees its target vanish.
        drop(self.capture.take());
        // A non-lingering adapter node lives as long as the resource bound to its proxy, so
        // destroying the proxy destroys the sink on the server.
        drop(self.sink.take());
    }
}

/// Sets (or with `None` deletes) the `target.object` entry of `node_id` and records it.
fn write_target(
    metadata: &Metadata,
    targets: &mut BTreeMap<u32, MetaValue>,
    node_id: u32,
    value: Option<MetaValue>,
) {
    match value {
        Some(v) => {
            metadata.set_property(node_id, TARGET_OBJECT, v.type_.as_deref(), Some(&v.value));
            targets.insert(node_id, v);
        }
        None => {
            metadata.set_property(node_id, TARGET_OBJECT, None, None);
            targets.remove(&node_id);
        }
    }
}

/// Deletes the `target.node` entry of `node_id` and records it.
fn clear_target_node(metadata: &Metadata, node_targets: &mut BTreeMap<u32, String>, node_id: u32) {
    metadata.set_property(node_id, TARGET_NODE, None, None);
    node_targets.remove(&node_id);
}
