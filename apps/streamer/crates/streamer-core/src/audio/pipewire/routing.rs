//! The per-run virtual sink and the routing of selected app streams to it (spec §9.3).
//! Runs on the PipeWire thread.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use ::pipewire as pw;
use pw::metadata::Metadata;
use pw::node::Node;
use pw::properties::{PropertiesBox, properties};

use super::ROUTE_DEBOUNCE;
use super::thread::{MetaValue, Reply};
use crate::audio::apps::{RouteAction, StreamNode, routing_actions};
use crate::audio::{AudioError, AudioEvent, AudioIo};

const TARGET_OBJECT: &str = "target.object";

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

/// One start..stop run: the sink, the selection and the streams moved to the sink.
pub(super) struct Session {
    pub sink_name: String,
    /// Dropping the proxy destroys the sink on the server.
    sink: Option<Node>,
    pub sink_proxy_id: u32,
    pub sink_global: Option<SinkGlobal>,
    selected: Vec<String>,
    /// Moved stream id -> its `target.object` metadata entry before the move.
    moved: BTreeMap<u32, Option<MetaValue>>,
    route_at: Option<Instant>,
    /// Answered when the sink shows up in the registry.
    pub pending_start: Option<(Instant, Reply<()>)>,
    failed: bool,
    /// Capture and playback ends; Task 12 uses them on this thread.
    io: AudioIo,
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
        Self {
            sink_name,
            sink: Some(sink),
            sink_proxy_id,
            sink_global: None,
            selected,
            moved: BTreeMap::new(),
            route_at: None,
            pending_start: Some((start_deadline, reply)),
            failed: false,
            io,
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
    }

    /// Reports a failure to the runtime once and stops routing.
    pub fn fail(&mut self, message: &str) {
        if !self.failed {
            self.failed = true;
            let _ = self.io.events.send(AudioEvent::Failed(message.to_string()));
        }
    }

    /// Moves streams of selected apps to the sink and restores unselected ones.
    /// `nodes` must not contain the streamer's own streams.
    pub fn route(
        &mut self,
        metadata: &Metadata,
        nodes: &[StreamNode],
        targets: &mut BTreeMap<u32, MetaValue>,
    ) {
        let Some(serial) = self.sink_global.as_ref().map(|s| s.serial.clone()) else {
            return;
        };
        let moved: BTreeSet<u32> = self.moved.keys().copied().collect();
        for action in routing_actions(&self.selected, nodes, &moved) {
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
                }
            }
        }
    }

    /// Gives every moved stream its previous target back (or deletes the entry) and deletes
    /// every other entry that points at the sink.
    pub fn restore_all(&mut self, metadata: &Metadata, targets: &mut BTreeMap<u32, MetaValue>) {
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
    }

    /// Answers a pending start with `result`.
    pub fn answer_start(&mut self, result: Result<(), AudioError>) {
        if let Some((_, reply)) = self.pending_start.take() {
            let _ = reply.send(result);
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.answer_start(Err(AudioError::Backend(
            "Audio routing stopped before the sink was ready".to_string(),
        )));
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
