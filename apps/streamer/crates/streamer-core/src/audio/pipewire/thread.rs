//! The PipeWire thread: main loop, context and core on a dedicated thread. Commands come in
//! through a `pipewire::channel`; replies go back through std channels, failures while
//! started through `AudioIo::events`, failures of the output through `OutputIo::events`.
//!
//! PipeWire callbacks only record what happened in an [`Inbox`]; [`Worker::work`] acts on it
//! after each loop iteration, so no PipeWire call runs inside a callback.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ::pipewire as pw;
use pw::client::{Client, ClientListener};
use pw::core::{CoreRc, PW_ID_CORE};
use pw::loop_::Timeout;
use pw::metadata::{Metadata, MetadataListener};
use pw::node::{Node, NodeChangeMask, NodeListener};
use pw::properties::{PropertiesBox, properties};
use pw::registry::{GlobalObject, RegistryRc};
use pw::spa::utils::dict::DictRef;
use pw::stream::StreamState;
use pw::types::ObjectType;

use super::capture::connect_capture;
use super::playback::{NO_LOCAL_OUTPUT, PlaybackStream, connect_playback, local_copy_allowed};
use super::routing::{Session, SinkGlobal, sink_name, sink_properties};
use super::{INIT_TIMEOUT, OWN_APP_NAME, SINK_TIMEOUT, STOP_TIMEOUT};
use crate::audio::apps::{MetaValue, StreamNode, WirePlumber, check_versions, group_apps};
use crate::audio::{AppInfo, AudioError, AudioEvent, AudioIo, OutputIo};
use crate::mixer::Mixer;

pub(super) type Reply<T> = mpsc::Sender<Result<T, AudioError>>;

/// The `default` metadata key (subject 0) of the default output.
const DEFAULT_SINK_KEY: &str = "default.audio.sink";

pub(super) enum Command {
    ListApps(Reply<Vec<AppInfo>>),
    Start {
        apps: Vec<String>,
        io: AudioIo,
        reply: Reply<()>,
    },
    UpdateApps {
        apps: Vec<String>,
        reply: Reply<()>,
    },
    Stop(Reply<()>),
    /// Opens the output, after closing an open one. Without `target` (a `node.name`) it
    /// follows the default output; tests pin it to their own sink.
    OpenOutput {
        io: OutputIo,
        target: Option<String>,
        reply: Reply<()>,
    },
    CloseOutput(Reply<()>),
    /// Stops routing if started, closes the output, then ends the thread.
    Quit,
    /// Writes a `target.object` entry for `subject` (tests provoke a refused write).
    #[cfg(test)]
    WriteTarget {
        subject: u32,
        reply: Reply<()>,
    },
    /// Creates a null sink named `name` that lives as long as the thread (a target that
    /// tests can play to without a sound).
    #[cfg(test)]
    CreateSink {
        name: String,
        reply: Reply<()>,
    },
    /// From now on the worker takes `value` as the `default.audio.sink` metadata value, as
    /// after a change of the default output. The real default output stays as it is.
    #[cfg(test)]
    FakeDefaultSink {
        value: String,
        reply: Reply<()>,
    },
    /// Destroys the session's sink through its proxy, with the capture still connected: the
    /// sink disappears as if another client removed it. PipeWire before 1.3.81 aborts when
    /// another client destroys a null sink whose owner is still connected (`pw-cli destroy`,
    /// PipeWire issue 4250).
    #[cfg(test)]
    DropSink(Reply<()>),
}

/// A command and when its caller stops waiting for the answer.
pub(super) struct Request {
    pub command: Command,
    /// After this a `Start`, `UpdateApps`, `OpenOutput` or `ListApps` is dropped unrun, so
    /// nothing happens that no caller hears about. `Stop`, `CloseOutput` and `Quit` only
    /// undo, so they always run.
    pub expires: Option<Instant>,
}

impl Request {
    fn is_expired(&self, now: Instant) -> bool {
        let undoes = matches!(
            self.command,
            Command::Stop(_) | Command::CloseOutput(_) | Command::Quit
        );
        !undoes && self.expires.is_some_and(|at| now >= at)
    }
}

enum GlobalEvent {
    Added(GlobalObject<PropertiesBox>),
    Removed(u32),
}

struct CoreError {
    id: u32,
    res: i32,
    message: String,
}

struct StreamEntry {
    node: StreamNode,
    process_id: Option<u32>,
}

/// What the PipeWire callbacks record.
#[derive(Default)]
struct Inbox {
    pw_version: Option<String>,
    wp_version: Option<String>,
    globals: Vec<GlobalEvent>,
    commands: VecDeque<Request>,
    /// `Stream/Output/Audio` nodes by registry id, with the props of the latest info.
    streams: BTreeMap<u32, StreamEntry>,
    streams_changed: bool,
    /// `target.object` entries of the `default` metadata by subject.
    targets: BTreeMap<u32, MetaValue>,
    /// Raw seqs of `done` events for `core.sync`.
    done: Vec<i32>,
    errors: Vec<CoreError>,
    /// New states of the capture and playback streams, by stream id.
    stream_states: Vec<(u64, StreamState)>,
    /// The `default.audio.sink` value of the `default` metadata (`{"name":"…"}`).
    default_sink: Option<String>,
    default_sink_changed: bool,
    /// A `target.object` entry changed.
    targets_changed: bool,
}

/// A proxy with its listener; the listener is dropped first.
struct Bound<P, L> {
    _listener: L,
    proxy: P,
}

struct DefaultMetadata {
    id: u32,
    /// Core errors about our writes carry this id.
    proxy_id: u32,
    bound: Bound<Metadata, MetadataListener>,
}

struct Init {
    reply: mpsc::Sender<Result<(), AudioError>>,
    deadline: Instant,
    /// Seq of the pending sync and its round: after round 1 every global is known, after
    /// round 2 the info of what round 1 bound.
    sync: Option<(i32, u8)>,
}

struct Stopping {
    sync: Option<i32>,
    deadline: Instant,
    replies: Vec<Reply<()>>,
}

/// The output: one playback stream for the whole connection, independent of the session.
struct Output {
    stream: PlaybackStream,
    /// Where its failure goes.
    events: tokio::sync::mpsc::UnboundedSender<AudioEvent>,
}

/// Runs the PipeWire thread until `Quit`. Sends the outcome of connecting and the version
/// check on `init`; on failure the thread ends by itself.
pub(super) fn run(
    command_rx: pw::channel::Receiver<Request>,
    init: mpsc::Sender<Result<(), AudioError>>,
) {
    let unavailable = |what: &str, e: pw::Error| AudioError::Unavailable(format!("{what}: {e}"));
    pw::init();
    let main_loop = match pw::main_loop::MainLoopRc::new(None) {
        Ok(l) => l,
        Err(e) => {
            let _ = init.send(Err(unavailable("cannot create a main loop", e)));
            return;
        }
    };
    let context = match pw::context::ContextRc::new(
        &main_loop,
        Some(properties! { "application.name" => OWN_APP_NAME }),
    ) {
        Ok(c) => c,
        Err(e) => {
            let _ = init.send(Err(unavailable("cannot create a context", e)));
            return;
        }
    };
    let core = match context.connect_rc(None) {
        Ok(c) => c,
        Err(e) => {
            let _ = init.send(Err(unavailable("cannot connect to the PipeWire server", e)));
            return;
        }
    };
    let registry = match core.get_registry_rc() {
        Ok(r) => r,
        Err(e) => {
            let _ = init.send(Err(unavailable("cannot get the registry", e)));
            return;
        }
    };

    let inbox = Rc::new(RefCell::new(Inbox::default()));
    let core_listener = core
        .add_listener_local()
        .info({
            let inbox = inbox.clone();
            move |info| inbox.borrow_mut().pw_version = Some(info.version().to_string())
        })
        .done({
            let inbox = inbox.clone();
            move |id, seq| {
                if id == PW_ID_CORE {
                    inbox.borrow_mut().done.push(seq.raw());
                }
            }
        })
        .error({
            let inbox = inbox.clone();
            move |id, _seq, res, message| {
                inbox.borrow_mut().errors.push(CoreError {
                    id,
                    res,
                    message: message.to_string(),
                })
            }
        })
        .register();
    let registry_listener = registry
        .add_listener_local()
        .global({
            let inbox = inbox.clone();
            move |global| {
                if is_relevant(global) {
                    let owned = GlobalEvent::Added(global.to_owned());
                    inbox.borrow_mut().globals.push(owned);
                }
            }
        })
        .global_remove({
            let inbox = inbox.clone();
            move |id| inbox.borrow_mut().globals.push(GlobalEvent::Removed(id))
        })
        .register();
    let attached = command_rx.attach(main_loop.loop_(), {
        let inbox = inbox.clone();
        move |request| inbox.borrow_mut().commands.push_back(request)
    });

    let mut worker = Worker::new(inbox, core.clone(), registry.clone(), init);
    let loop_ = main_loop.loop_();
    // SAFETY: paired with the `leave` below; `iterate_unguarded` only runs in between, on this
    // thread, like `pw_main_loop_run` does.
    unsafe { loop_.enter() };
    while !worker.finished() {
        let timeout = match worker.next_deadline() {
            Some(at) => {
                let wait = at.saturating_duration_since(Instant::now());
                // Round up to whole ms so a sub-ms wait does not spin.
                Timeout::Finite(Duration::from_millis(wait.as_micros().div_ceil(1000) as u64))
            }
            None => Timeout::Infinite,
        };
        // SAFETY: inside `enter`/`leave` (see above).
        unsafe { loop_.iterate_unguarded(timeout) };
        worker.work();
    }
    // SAFETY: pairs with `enter` above.
    unsafe { loop_.leave() };

    // Listeners before the proxies they listen on; proxies before the core.
    drop(worker);
    drop(attached);
    drop(registry_listener);
    drop(core_listener);
    drop(registry);
    drop(core);
    drop(context);
    drop(main_loop);
}

/// Globals the worker uses: nodes (streams and the sink), the `default` metadata and the
/// WirePlumber client.
fn is_relevant(global: &GlobalObject<&DictRef>) -> bool {
    let Some(props) = global.props else {
        return false;
    };
    match global.type_ {
        ObjectType::Node => true,
        ObjectType::Metadata => props.get("metadata.name") == Some("default"),
        ObjectType::Client => props.get("application.name") == Some("WirePlumber"),
        _ => false,
    }
}

fn is_start(request: &Request) -> bool {
    matches!(request.command, Command::Start { .. })
}

fn is_stream(props: &DictRef) -> bool {
    props.get("media.class") == Some("Stream/Output/Audio")
}

fn update_stream(entry: &mut StreamEntry, props: &DictRef) {
    let get = |key: &str| props.get(key).filter(|v| !v.is_empty()).map(str::to_string);
    if let Some(serial) = props.get("object.serial").and_then(|s| s.parse().ok()) {
        entry.node.serial = serial;
    }
    entry.node.app_name = get("application.name");
    entry.node.binary = get("application.process.binary");
    entry.node.node_name = get("node.name");
    entry.node.target_object = get("target.object");
    entry.process_id = props
        .get("application.process.id")
        .and_then(|p| p.parse().ok());
}

struct Worker {
    inbox: Rc<RefCell<Inbox>>,
    core: CoreRc,
    registry: RegistryRc,
    streams: BTreeMap<u32, Bound<Node, NodeListener>>,
    wireplumber: BTreeMap<u32, Bound<Client, ClientListener>>,
    metadata: Option<DefaultMetadata>,
    init: Option<Init>,
    session: Option<Session>,
    stopping: Option<Stopping>,
    output: Option<Output>,
    /// Set when the connection to PipeWire is gone.
    lost: Option<String>,
    quit: bool,
    sinks_created: u32,
    /// Capture and playback streams created, for their ids.
    media_created: u64,
    /// Sinks of [`Command::CreateSink`]; dropping a proxy destroys its sink.
    #[cfg(test)]
    test_sinks: Vec<Node>,
    /// The `default.audio.sink` value of [`Command::FakeDefaultSink`].
    #[cfg(test)]
    fake_default_sink: Option<String>,
}

impl Worker {
    fn new(
        inbox: Rc<RefCell<Inbox>>,
        core: CoreRc,
        registry: RegistryRc,
        reply: mpsc::Sender<Result<(), AudioError>>,
    ) -> Self {
        let mut worker = Self {
            inbox,
            core,
            registry,
            streams: BTreeMap::new(),
            wireplumber: BTreeMap::new(),
            metadata: None,
            init: Some(Init {
                reply,
                deadline: Instant::now() + INIT_TIMEOUT,
                sync: None,
            }),
            session: None,
            stopping: None,
            output: None,
            lost: None,
            quit: false,
            sinks_created: 0,
            media_created: 0,
            #[cfg(test)]
            test_sinks: Vec::new(),
            #[cfg(test)]
            fake_default_sink: None,
        };
        let seq = worker.sync();
        if let Some(init) = &mut worker.init {
            init.sync = seq.map(|s| (s, 1));
        }
        worker
    }

    fn finished(&self) -> bool {
        self.quit && self.stopping.is_none()
    }

    fn next_deadline(&self) -> Option<Instant> {
        if self.commands_ready() {
            return Some(Instant::now());
        }
        let session = self.session.as_ref();
        [
            self.init.as_ref().map(|i| i.deadline),
            session.and_then(|s| s.pending_start.as_ref().map(|(at, _)| *at)),
            session.and_then(Session::route_at),
            self.stopping.as_ref().map(|s| s.deadline),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Whether a queued command can run now (a `Start` waits while a stop is in progress).
    fn commands_ready(&self) -> bool {
        let inbox = self.inbox.borrow();
        match inbox.commands.front() {
            None => false,
            Some(request) => !(self.stopping.is_some() && is_start(request)),
        }
    }

    fn sync(&self) -> Option<i32> {
        self.core.sync(0).ok().map(|seq| seq.raw())
    }

    fn work(&mut self) {
        let now = Instant::now();
        let (errors, globals, done, streams_changed, stream_states, default_changed) = {
            let mut inbox = self.inbox.borrow_mut();
            (
                std::mem::take(&mut inbox.errors),
                std::mem::take(&mut inbox.globals),
                std::mem::take(&mut inbox.done),
                std::mem::replace(&mut inbox.streams_changed, false),
                std::mem::take(&mut inbox.stream_states),
                std::mem::replace(&mut inbox.default_sink_changed, false),
            )
        };
        let targets_changed =
            std::mem::replace(&mut self.inbox.borrow_mut().targets_changed, false);
        let routing_inputs_changed = !globals.is_empty() || streams_changed || targets_changed;
        for error in errors {
            self.on_error(error);
        }
        for event in globals {
            match event {
                GlobalEvent::Added(global) => self.on_global(&global, now),
                GlobalEvent::Removed(id) => self.on_global_removed(id, now),
            }
        }
        if streams_changed && let Some(session) = &mut self.session {
            session.mark_dirty(now);
        }
        self.on_stream_states(stream_states);
        if default_changed {
            self.guard_local_output();
        }
        self.progress_init(&done, now);
        self.handle_commands(now);
        self.progress_start(now);
        if routing_inputs_changed {
            self.route_at_once(now);
        }
        self.route_if_due(now);
        self.progress_stop(&done, now);
        // A start that waited for a stop finished above runs now, not at some later wake-up.
        self.handle_commands(now);
    }

    fn on_error(&mut self, error: CoreError) {
        if error.id == PW_ID_CORE && error.res == -libc::EPIPE {
            let message = "The connection to PipeWire was lost";
            self.lost = Some(message.to_string());
            if let Some(session) = &mut self.session {
                session.fail(message);
            }
            self.fail_output("the connection to PipeWire was lost".to_string());
            return;
        }
        if let Some(session) = &mut self.session
            && session.pending_start.is_some()
            && error.id == session.sink_proxy_id
        {
            let message = format!("Cannot create the Beatsync sink: {}", error.message);
            session.answer_start(Err(AudioError::Backend(message)));
            self.session = None;
            return;
        }
        if self
            .metadata
            .as_ref()
            .is_some_and(|m| m.proxy_id == error.id)
        {
            let message = format!("PipeWire refused a routing change: {}", error.message);
            match &mut self.session {
                Some(session) => session.fail(&message),
                None => eprintln!("streamer: {message}"),
            }
            return;
        }
        eprintln!(
            "streamer: PipeWire error on object {} ({}): {}",
            error.id, error.res, error.message
        );
    }

    fn on_global(&mut self, global: &GlobalObject<PropertiesBox>, now: Instant) {
        let Some(props) = global.props.as_ref() else {
            return;
        };
        let props: &DictRef = props.as_ref();
        match global.type_ {
            ObjectType::Node if is_stream(props) => self.bind_stream(global, props, now),
            ObjectType::Node => {
                if let Some(session) = &mut self.session
                    && props.get("node.name") == Some(session.sink_name.as_str())
                    && let Some(serial) = props.get("object.serial")
                {
                    session.sink_global = Some(SinkGlobal {
                        id: global.id,
                        serial: serial.to_string(),
                    });
                }
            }
            ObjectType::Metadata => self.bind_metadata(global),
            ObjectType::Client => self.bind_wireplumber(global),
            _ => {}
        }
    }

    fn bind_stream(&mut self, global: &GlobalObject<PropertiesBox>, props: &DictRef, now: Instant) {
        let Ok(proxy) = self.registry.bind::<Node, _>(global) else {
            return;
        };
        let listener = proxy
            .add_listener_local()
            .info({
                let inbox = self.inbox.clone();
                move |info| {
                    // Props are only sent when they changed; other updates carry none.
                    if !info.change_mask().contains(NodeChangeMask::PROPS) {
                        return;
                    }
                    let Some(props) = info.props() else { return };
                    let mut inbox = inbox.borrow_mut();
                    if let Some(entry) = inbox.streams.get_mut(&info.id()) {
                        update_stream(entry, props);
                        inbox.streams_changed = true;
                    }
                }
            })
            .register();
        let mut entry = StreamEntry {
            node: StreamNode {
                id: global.id,
                ..StreamNode::default()
            },
            process_id: None,
        };
        update_stream(&mut entry, props);
        self.inbox.borrow_mut().streams.insert(global.id, entry);
        self.streams.insert(
            global.id,
            Bound {
                _listener: listener,
                proxy,
            },
        );
        if let Some(session) = &mut self.session {
            session.note_added(global.id, now);
            session.mark_dirty(now);
        }
    }

    fn bind_metadata(&mut self, global: &GlobalObject<PropertiesBox>) {
        let Ok(proxy) = self.registry.bind::<Metadata, _>(global) else {
            return;
        };
        let listener = proxy
            .add_listener_local()
            .property({
                let inbox = self.inbox.clone();
                move |subject, key, type_, value| {
                    let mut inbox = inbox.borrow_mut();
                    let inbox = &mut *inbox;
                    if subject == 0 && matches!(key, Some(DEFAULT_SINK_KEY) | None) {
                        inbox.default_sink = value.filter(|_| key.is_some()).map(str::to_string);
                        inbox.default_sink_changed = true;
                    }
                    let targets = &mut inbox.targets;
                    match (key, value) {
                        // `clear()` of the whole metadata, or of one subject.
                        (None, _) if subject == pw::constants::ID_ANY => {
                            targets.clear();
                            inbox.default_sink = None;
                            inbox.default_sink_changed = true;
                            inbox.targets_changed = true;
                        }
                        (None, _) => {
                            targets.remove(&subject);
                            inbox.targets_changed = true;
                        }
                        (Some("target.object"), Some(value)) => {
                            let entry = MetaValue {
                                type_: type_.map(str::to_string),
                                value: value.to_string(),
                            };
                            targets.insert(subject, entry);
                            inbox.targets_changed = true;
                        }
                        (Some("target.object"), None) => {
                            targets.remove(&subject);
                            inbox.targets_changed = true;
                        }
                        _ => {}
                    }
                    0
                }
            })
            .register();
        {
            let mut inbox = self.inbox.borrow_mut();
            inbox.targets.clear();
            inbox.default_sink = None;
        }
        self.metadata = Some(DefaultMetadata {
            id: global.id,
            proxy_id: pw::proxy::ProxyT::upcast_ref(&proxy).id(),
            bound: Bound {
                _listener: listener,
                proxy,
            },
        });
    }

    fn bind_wireplumber(&mut self, global: &GlobalObject<PropertiesBox>) {
        let Ok(proxy) = self.registry.bind::<Client, _>(global) else {
            return;
        };
        let listener = proxy
            .add_listener_local()
            .info({
                let inbox = self.inbox.clone();
                move |info| {
                    let version = info.props().and_then(|p| p.get("application.version"));
                    if let Some(version) = version {
                        inbox.borrow_mut().wp_version = Some(version.to_string());
                    }
                }
            })
            .register();
        self.wireplumber.insert(
            global.id,
            Bound {
                _listener: listener,
                proxy,
            },
        );
    }

    fn on_global_removed(&mut self, id: u32, now: Instant) {
        if self.streams.remove(&id).is_some() {
            self.inbox.borrow_mut().streams.remove(&id);
            if let Some(session) = &mut self.session {
                session.forget(id);
                session.mark_dirty(now);
            }
        }
        self.wireplumber.remove(&id);
        if self.metadata.as_ref().is_some_and(|m| m.id == id) {
            self.metadata = None;
            self.inbox.borrow_mut().targets.clear();
            if let Some(session) = &mut self.session {
                session.fail("The PipeWire session manager stopped");
            }
        }
        if let Some(session) = &mut self.session
            && session.sink_global.as_ref().is_some_and(|s| s.id == id)
        {
            session.fail("The Beatsync sink was removed");
        }
    }

    fn progress_init(&mut self, done: &[i32], now: Instant) {
        let Some(init) = &self.init else { return };
        let synced_round = init
            .sync
            .filter(|(seq, _)| done.contains(seq))
            .map(|(_, round)| round);
        if synced_round == Some(1) {
            let seq = self.sync();
            if let Some(init) = &mut self.init {
                init.sync = seq.map(|s| (s, 2));
            }
            return;
        }
        if synced_round.is_none() && now < init.deadline && self.lost.is_none() {
            return;
        }
        let Some(init) = self.init.take() else { return };
        let result = {
            let inbox = self.inbox.borrow();
            match (&self.lost, &inbox.pw_version) {
                (Some(lost), _) => Err(AudioError::Unavailable(lost.clone())),
                (None, None) => Err(AudioError::Unavailable(
                    "the PipeWire server did not answer".to_string(),
                )),
                (None, Some(version)) => {
                    let wireplumber = match (self.wireplumber.is_empty(), &inbox.wp_version) {
                        (true, _) => WirePlumber::Missing,
                        (false, Some(version)) => WirePlumber::Version(version),
                        (false, None) => WirePlumber::Unversioned,
                    };
                    check_versions(version, wireplumber)
                }
            }
        };
        if result.is_err() {
            self.quit = true;
        }
        let _ = init.reply.send(result);
    }

    fn handle_commands(&mut self, now: Instant) {
        loop {
            let request = {
                let mut inbox = self.inbox.borrow_mut();
                // A start waits until the previous stop is done.
                if self.stopping.is_some() && inbox.commands.front().is_some_and(is_start) {
                    break;
                }
                inbox.commands.pop_front()
            };
            let Some(request) = request else { break };
            if request.is_expired(now) {
                continue;
            }
            match request.command {
                Command::ListApps(reply) => {
                    let _ = reply.send(self.list_apps());
                }
                Command::Start { apps, io, reply } => self.start(apps, io, reply, now),
                Command::UpdateApps { apps, reply } => {
                    if let Some(session) = &mut self.session {
                        session.select(apps, now);
                    }
                    let _ = reply.send(Ok(()));
                }
                Command::Stop(reply) => self.begin_stop(Some(reply), now),
                Command::OpenOutput { io, target, reply } => {
                    let _ = reply.send(self.open_output(io, target.as_deref()));
                }
                Command::CloseOutput(reply) => {
                    self.output = None;
                    let _ = reply.send(Ok(()));
                }
                Command::Quit => {
                    self.quit = true;
                    self.begin_stop(None, now);
                    self.output = None;
                }
                #[cfg(test)]
                Command::WriteTarget { subject, reply } => {
                    if let Some(metadata) = &self.metadata {
                        let proxy = &metadata.bound.proxy;
                        proxy.set_property(subject, "target.object", Some("Spa:Id"), Some("0"));
                    }
                    let _ = reply.send(Ok(()));
                }
                #[cfg(test)]
                Command::CreateSink { name, reply } => {
                    let sink = self
                        .core
                        .create_object::<Node>("adapter", &sink_properties(&name));
                    let result = sink.map(|sink| self.test_sinks.push(sink));
                    let _ = reply.send(result.map_err(|e| AudioError::Backend(e.to_string())));
                }
                #[cfg(test)]
                Command::FakeDefaultSink { value, reply } => {
                    self.fake_default_sink = Some(value);
                    self.guard_local_output();
                    let _ = reply.send(Ok(()));
                }
                #[cfg(test)]
                Command::DropSink(reply) => {
                    if let Some(session) = &mut self.session {
                        session.drop_sink();
                    }
                    let _ = reply.send(Ok(()));
                }
            }
        }
    }

    /// Streams other than the streamer's own (by app name or process).
    fn foreign_streams(&self) -> Vec<StreamNode> {
        let own_pid = std::process::id();
        self.inbox
            .borrow()
            .streams
            .values()
            .filter(|e| e.process_id != Some(own_pid))
            .filter(|e| e.node.app_name.as_deref() != Some(OWN_APP_NAME))
            .map(|e| e.node.clone())
            .collect()
    }

    fn list_apps(&self) -> Result<Vec<AppInfo>, AudioError> {
        if let Some(lost) = &self.lost {
            return Err(AudioError::Backend(lost.clone()));
        }
        Ok(group_apps(&self.foreign_streams(), OWN_APP_NAME))
    }

    fn start(&mut self, apps: Vec<String>, io: AudioIo, reply: Reply<()>, now: Instant) {
        let refusal = if let Some(lost) = &self.lost {
            Some(AudioError::Backend(lost.clone()))
        } else if self.session.is_some() {
            Some(AudioError::AlreadyStarted)
        } else if self.metadata.is_none() {
            Some(AudioError::Backend(
                "WirePlumber's default metadata is missing".to_string(),
            ))
        } else {
            None
        };
        if let Some(error) = refusal {
            let _ = reply.send(Err(error));
            return;
        }
        self.sinks_created += 1;
        let sink_name = sink_name(std::process::id(), self.sinks_created);
        let sink = match self
            .core
            .create_object::<Node>("adapter", &sink_properties(&sink_name))
        {
            Ok(sink) => sink,
            Err(e) => {
                let message = format!("Cannot create the Beatsync sink: {e}");
                let _ = reply.send(Err(AudioError::Backend(message)));
                return;
            }
        };
        self.session = Some(Session::new(
            sink_name,
            sink,
            apps,
            io,
            now + SINK_TIMEOUT,
            reply,
        ));
    }

    /// Answers the start once the sink is in the registry, after the first routing.
    fn progress_start(&mut self, now: Instant) {
        let Some(session) = &mut self.session else {
            return;
        };
        let Some((deadline, _)) = &session.pending_start else {
            return;
        };
        if session.sink_global.is_some() {
            if session.answer_start(Ok(())) {
                self.route(now);
                self.start_media();
            } else {
                // The caller stopped waiting: undo rather than keep a session nobody owns.
                self.begin_stop(None, now);
            }
        } else if now >= *deadline {
            session.answer_start(Err(AudioError::Backend(
                "The Beatsync sink did not appear".to_string(),
            )));
            self.session = None;
        }
    }

    fn next_media_id(&mut self) -> u64 {
        self.media_created += 1;
        self.media_created
    }

    /// Records the stream's new states for [`Self::on_stream_states`].
    fn state_recorder(&self, id: u64) -> impl FnMut(StreamState) + 'static {
        let inbox = self.inbox.clone();
        move |state| {
            // A stream can change state while the worker holds the inbox (it disconnects a
            // stream); that state is of a stream being dropped, so losing it is fine.
            if let Ok(mut inbox) = inbox.try_borrow_mut() {
                inbox.stream_states.push((id, state));
            }
        }
    }

    /// Creates the capture of the sink's monitor once the sink exists and the first routing
    /// ran. When the default output is one of our sinks, the session fails with no capture:
    /// the output, which follows the default output, would play into the capture.
    fn start_media(&mut self) {
        let capture_id = self.next_media_id();
        let on_capture_state = self.state_recorder(capture_id);
        let allowed = self.default_output_allowed();
        let Some(session) = &mut self.session else {
            return;
        };
        if !session.is_routing() {
            return;
        }
        if !allowed {
            session.fail(NO_LOCAL_OUTPUT);
            return;
        }
        let Some(capture_tx) = session.take_capture_tx() else {
            return;
        };
        let capture = connect_capture(
            &self.core,
            &session.sink_name,
            capture_tx,
            session.shared.clone(),
            capture_id,
            on_capture_state,
        );
        match capture {
            Ok(stream) => session.capture = Some(stream),
            Err(e) => session.fail(&format!("Cannot capture the Beatsync sink: {e}")),
        }
    }

    /// Whether the default output is not one of this process's sinks.
    fn default_output_allowed(&self) -> bool {
        #[cfg(test)]
        if let Some(value) = &self.fake_default_sink {
            return local_copy_allowed(Some(value), std::process::id());
        }
        let inbox = self.inbox.borrow();
        local_copy_allowed(inbox.default_sink.as_deref(), std::process::id())
    }

    /// Opens the output with a [`Mixer`] of `io`, after closing an open one.
    fn open_output(&mut self, io: OutputIo, target: Option<&str>) -> Result<(), AudioError> {
        self.output = None;
        if let Some(lost) = &self.lost {
            return Err(AudioError::Backend(lost.clone()));
        }
        let id = self.next_media_id();
        let on_state = self.state_recorder(id);
        let OutputIo {
            stream_rx,
            track_rx,
            metronome,
            lowpass,
            shared,
            events,
        } = io;
        let mixer = Mixer::new(stream_rx, track_rx, metronome, shared.clone(), lowpass);
        let stream = connect_playback(&self.core, target, mixer, shared, id, on_state)
            .map_err(AudioError::Backend)?;
        self.output = Some(Output { stream, events });
        // While a session streams, the rule for a change of the default output applies.
        self.guard_local_output();
        Ok(())
    }

    /// Closes the output and reports `reason`; a running session goes on.
    fn fail_output(&mut self, reason: String) {
        if let Some(Output { stream, events }) = self.output.take() {
            drop(stream);
            let _ = events.send(AudioEvent::OutputFailed(reason));
        }
    }

    /// A stream error, or a stream the server disconnected: the dead stream is dropped at
    /// once, so it sends nothing more to the server. A failed capture fails the session; a
    /// failed output closes only the output.
    fn on_stream_states(&mut self, states: Vec<(u64, StreamState)>) {
        for (id, state) in states {
            let (reason, output_reason) = match state {
                StreamState::Error(message) => (format!("failed: {message}"), message),
                StreamState::Unconnected => (
                    "was disconnected".to_string(),
                    "the stream was disconnected".to_string(),
                ),
                StreamState::Connecting | StreamState::Paused | StreamState::Streaming => continue,
            };
            if self.output.as_ref().is_some_and(|o| o.stream.id == id) {
                self.fail_output(output_reason);
            } else if let Some(session) = &mut self.session
                && let Some(label) = session.remove_stream(id)
            {
                session.fail(&format!("{label} {reason}"));
            }
        }
    }

    /// The default output became one of our sinks (for example, the last real device went
    /// away): the output, which follows it, now plays into the capture. The capture stops at
    /// once, so no frame of the PC's own output reaches the room, and the session fails; its
    /// stop restores the routing and removes the sink. The output stays open.
    fn guard_local_output(&mut self) {
        if self.default_output_allowed() {
            return;
        }
        if let Some(session) = &mut self.session
            && session.capture.is_some()
        {
            session.fail(NO_LOCAL_OUTPUT);
            drop(session.capture.take());
        }
    }

    /// Routes now, not after the debounce, when a stream of a selected app is not on the sink
    /// yet (see [`Session::wants_route_now`]). Called after streams or targets changed.
    fn route_at_once(&mut self, now: Instant) {
        let Some(session) = &self.session else { return };
        let nodes = self.foreign_streams();
        let wants = session.wants_route_now(&nodes, &self.inbox.borrow().targets, now);
        if wants {
            self.route(now);
        }
    }

    fn route_if_due(&mut self, now: Instant) {
        if let Some(session) = &mut self.session
            && session.take_due(now)
        {
            self.route(now);
        }
    }

    fn route(&mut self, now: Instant) {
        let nodes = self.foreign_streams();
        let (Some(session), Some(metadata)) = (&mut self.session, &self.metadata) else {
            return;
        };
        if session.is_routing() {
            let mut inbox = self.inbox.borrow_mut();
            session.route(&metadata.bound.proxy, &nodes, &mut inbox.targets, now);
        }
    }

    /// Restores routing, destroys the sink and answers once the server confirms both.
    fn begin_stop(&mut self, reply: Option<Reply<()>>, now: Instant) {
        if let Some(stopping) = &mut self.stopping {
            stopping.replies.extend(reply);
            return;
        }
        let Some(mut session) = self.session.take() else {
            if let Some(reply) = reply {
                let _ = reply.send(Ok(()));
            }
            return;
        };
        if self.lost.is_none()
            && let Some(metadata) = &self.metadata
        {
            let mut inbox = self.inbox.borrow_mut();
            session.restore_all(&metadata.bound.proxy, &mut inbox.targets, now);
        }
        drop(session);
        let sync = if self.lost.is_none() {
            self.sync()
        } else {
            None
        };
        self.stopping = Some(Stopping {
            sync,
            deadline: now + STOP_TIMEOUT,
            replies: reply.into_iter().collect(),
        });
    }

    fn progress_stop(&mut self, done: &[i32], now: Instant) {
        let Some(stopping) = &self.stopping else {
            return;
        };
        let confirmed = stopping.sync.is_none_or(|seq| done.contains(&seq));
        if !confirmed && now < stopping.deadline && self.lost.is_none() {
            return;
        }
        let Some(stopping) = self.stopping.take() else {
            return;
        };
        let result = if confirmed || self.lost.is_some() {
            Ok(())
        } else {
            Err(AudioError::Backend(
                "PipeWire did not confirm the stop".to_string(),
            ))
        };
        for reply in stopping.replies {
            let _ = reply.send(result.clone());
        }
    }
}
