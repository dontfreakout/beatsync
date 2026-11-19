//! Streamer runtime (spec §9.5–§9.6, §10.1): wires the engine, the WebSocket connection and
//! the audio backend.
//!
//! - The engine task owns the [`Engine`]. It feeds it a 10 ms tick, the connection's events,
//!   the user's commands and the audio worker's encoded blocks, and executes its outputs.
//! - The connection task is [`run_connection`]; it ends when the engine task drops its
//!   [`Outgoing`] sender at shutdown.
//! - The audio task runs the backend calls one after another through `spawn_blocking` (each
//!   can block for seconds), so a stop never overtakes the start before it.
//! - The audio worker (one std thread per audio session) cuts captured chunks into 20 ms
//!   blocks, stamps them in server time, encodes them for the room and queues the raw PCM for
//!   the local copy.
//! - The track loader (app only) downloads the current queue tracks into the cache dir and
//!   reads their durations; the engine task feeds it the room messages.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle as ThreadHandle;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use url::Url;

use crate::audio::{
    AppInfo, AudioBackend, AudioError, AudioEvent, AudioIo, CHUNK_MAX_FRAMES, CapturedChunk,
    PlayoutShared,
};
use crate::clock::{MonoEpochMap, clock_step_ms, mono_ns};
use crate::config::{Settings, track_cache_dir};
use crate::connection::{ConnEvent, JoinKind, Outgoing, build_ws_url, run_connection};
use crate::encoder::OpusFrameEncoder;
use crate::engine::{Engine, Input, Output, Phase, Status};
use crate::frame::{FRAME_DURATION_MS, FRAME_SAMPLES, MAX_DELAY_MS, MIN_DELAY_MS, SAMPLE_RATE};
use crate::http::{HttpError, client, download, http_base, resolve_audio_url};
use crate::media::probe_duration;
use crate::playout::{BLOCK_SAMPLES, CHANNELS, PcmBlock};
use crate::room;
use crate::timeline::CaptureTimeline;
use crate::tracks::{TrackAction, TrackPlanner, TrackStates, track_file_name};

const TICK: Duration = Duration::from_millis(10);
/// The mono→epoch mapping is measured again this often; a wall-clock step shows up as a jump
/// of the mapping at most this long after it happened.
const MONO_EPOCH_INTERVAL: Duration = Duration::from_secs(1);
/// 20 s of frames: more than a stalled socket lasts before its 5 s write timeout.
const OUTGOING_CAPACITY: usize = 1024;
/// Encoded blocks on their way from the worker to the engine.
const BLOCKS_CAPACITY: usize = 256;
/// A chunk is one graph quantum (split only above 1024 frames), so the ring is sized for the
/// smallest quantum: 384 chunks hold 0.5 s even at 64 frames per chunk (about 3 MB).
const CAPTURE_RING: usize = 384;
/// 512 blocks of 20 ms: the largest delay (5 s) plus 1 s, with room to spare.
const PLAYOUT_RING: usize = 512;
const TIMELINE_ALPHA: f64 = 0.002;
const TIMELINE_REANCHOR_MS: f64 = 50.0;
/// A chunk that starts further than this from the end of the previous chunk and the lost
/// frames restarts the block and the timeline (a capture restart).
const CAPTURE_GAP_NS: i64 = 50_000_000;
/// A shorter loss of captured audio plays as silence. A longer loss restarts the block and the
/// timeline: the timestamps then step by far more than the 50 ms at which the web receiver
/// resyncs, so every device resyncs, and no burst of silent blocks goes out.
const MAX_SILENT_FILL_NS: i64 = 200_000_000;
/// The worker sleeps this long when the capture ring is empty.
const WORKER_IDLE: Duration = Duration::from_millis(5);
/// At shutdown, how long queued messages (e.g. STOP_LIVE_STREAM) may take to go out.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
/// K4: a larger track is not downloaded; it shows "Track is larger than 200 MB".
const MAX_TRACK_BYTES: u64 = 200_000_000;

/// Server time at which the local copy plays the first frame of a block stamped `stamp_ms`:
/// the room's delay plus the encoder lookahead (receivers hear the decoded Opus that late)
/// minus the nudge (positive plays earlier, like the web client).
pub fn local_target_ms(stamp_ms: f64, delay_ms: u32, lookahead_samples: u32, nudge_ms: f64) -> f64 {
    let lookahead_ms = f64::from(lookahead_samples) * 1000.0 / f64::from(SAMPLE_RATE);
    stamp_ms + f64::from(delay_ms) + lookahead_ms - nudge_ms
}

/// A running streamer: one server connection, and an audio session while streaming.
pub struct StreamerHandle {
    commands: mpsc::UnboundedSender<Command>,
    status: watch::Receiver<Status>,
    tracks: watch::Receiver<TrackStates>,
    backend: Arc<dyn AudioBackend>,
    /// `None` when the server URL is invalid (the status message says why).
    task: Option<JoinHandle<()>>,
}

enum Command {
    Start {
        apps: Vec<String>,
        delay_ms: u32,
    },
    UpdateApps {
        apps: Vec<String>,
    },
    SetNudge {
        nudge_ms: f64,
    },
    /// A room action of the window, already checked.
    Action(serde_json::Value),
    /// Answered when the audio task has done every call queued before it.
    Stop(oneshot::Sender<()>),
    Shutdown,
}

impl StreamerHandle {
    /// Connects to the room in `settings` (and reconnects until [`Self::shutdown`]). An
    /// invalid server URL gives a handle that only reports the error in its status.
    ///
    /// `room_feed` gets the raw room messages for the window ([`room::forwarded`]); the CLI
    /// passes `None`. The app (`JoinKind::App`) also restores the server's stored nudge, asks
    /// for the room's playback state and downloads the current tracks ([`Self::tracks`]).
    pub async fn connect(
        settings: Settings,
        backend: Arc<dyn AudioBackend>,
        kind: JoinKind,
        room_feed: Option<mpsc::UnboundedSender<serde_json::Value>>,
    ) -> Self {
        Self::connect_in(settings, backend, kind, room_feed, track_cache_dir()).await
    }

    /// [`Self::connect`] with the track cache in `cache_dir`.
    async fn connect_in(
        settings: Settings,
        backend: Arc<dyn AudioBackend>,
        kind: JoinKind,
        room_feed: Option<mpsc::UnboundedSender<serde_json::Value>>,
        cache_dir: PathBuf,
    ) -> Self {
        let (tracks_tx, tracks) = watch::channel(TrackStates::new());
        let (status_tx, status) = watch::channel(Status {
            connected: false,
            synced: false,
            synced_once: false,
            rtt_ms: None,
            offset_ms: None,
            sync_measurements: 0,
            nudge_ms: settings.nudge_ms,
            listeners: 0,
            phase: Phase::Idle,
            message: None,
        });
        let (commands, commands_rx) = mpsc::unbounded_channel();
        let url = match build_ws_url(
            &settings.server_url,
            &settings.room_id,
            &settings.username,
            &settings.client_id,
            kind,
        ) {
            Ok(url) => url,
            Err(e) => {
                status_tx.send_modify(|s| s.message = Some(e.to_string()));
                return Self {
                    commands,
                    status,
                    tracks,
                    backend,
                    task: None,
                };
            }
        };

        let shared = Arc::new(PlayoutShared::default());
        let mono_epoch_offset_ms = MonoEpochMap::measure().to_epoch_ms(0);
        // No server offset yet: nothing is stamped before the first one.
        store_f64(&shared.mono_server_offset_ms, mono_epoch_offset_ms);
        // Full volume until the room says otherwise (the default 0 is silence).
        shared.gain.store(1.0_f32.to_bits(), Ordering::Relaxed);
        shared
            .gain_ramp_s
            .store(0.0_f32.to_bits(), Ordering::Relaxed);
        store_f64(&shared.nudge_ms, settings.nudge_ms);
        let offset_known = Arc::new(AtomicBool::new(false));

        let (out_tx, out_rx) = mpsc::channel(OUTGOING_CAPACITY);
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let connection = tokio::spawn(run_connection(url, out_rx, ev_tx, shutdown_rx));
        // The CLI downloads nothing and leaves the cache dir, which it shares with the app,
        // alone.
        let (loader_feed, loader) = match kind {
            JoinKind::App => match http_base(&settings.server_url) {
                Ok(base) => {
                    let (feed, messages) = mpsc::unbounded_channel();
                    let loader = TrackLoader::spawn(base, cache_dir, tracks_tx, messages);
                    (Some(feed), Some(loader))
                }
                Err(e) => {
                    eprintln!("streamer: no track downloads: {e}");
                    (None, None)
                }
            },
            JoinKind::Streamer => (None, None),
        };

        let (blocks_tx, blocks_rx) = mpsc::channel(BLOCKS_CAPACITY);
        let (notes_tx, notes_rx) = mpsc::unbounded_channel();
        let (audio_tx, audio_rx) = mpsc::unbounded_channel();
        let audio = tokio::spawn(audio_task(
            audio_rx,
            AudioContext {
                backend: backend.clone(),
                shared: shared.clone(),
                offset_known: offset_known.clone(),
                blocks: blocks_tx,
                notes: notes_tx,
            },
        ));

        let engine = EngineTask {
            engine: Engine::new(
                settings.client_id,
                settings.username,
                settings.nudge_ms,
                kind == JoinKind::App,
            ),
            out_tx: Some(out_tx),
            room_feed,
            loader_feed,
            audio_tx: Some(audio_tx),
            status_tx,
            shared,
            mono_epoch_offset_ms,
            server_offset_ms: 0.0,
            offset_known,
            next_session_id: 0,
            active_session_id: None,
            outgoing_dropped: 0,
        };
        let task = tokio::spawn(engine.run(
            Channels {
                commands: commands_rx,
                events: ev_rx,
                blocks: blocks_rx,
                notes: notes_rx,
            },
            Tasks {
                connection,
                shutdown_tx,
                audio,
                loader,
            },
        ));
        Self {
            commands,
            status,
            tracks,
            backend,
            task: Some(task),
        }
    }

    /// Routes `apps` into the sink and streams them once synced. Ignored unless idle.
    pub fn start(&self, apps: Vec<String>, delay_ms: u32) {
        let delay_ms = delay_ms.clamp(MIN_DELAY_MS, MAX_DELAY_MS);
        let _ = self.commands.send(Command::Start { apps, delay_ms });
    }

    pub fn update_apps(&self, apps: Vec<String>) {
        let _ = self.commands.send(Command::UpdateApps { apps });
    }

    /// Sets the nudge: the next block of the local copy uses it, and the server learns it.
    pub fn set_nudge(&self, nudge_ms: f64) {
        let _ = self.commands.send(Command::SetNudge { nudge_ms });
    }

    /// Ends the stream and returns when the routing is restored.
    pub async fn stop(&self) {
        let (done, wait) = oneshot::channel();
        if self.commands.send(Command::Stop(done)).is_ok() {
            let _ = wait.await;
        }
    }

    /// Sends a room action of the window. Refused while disconnected (it would be lost) and
    /// for the types the window may not send ([`room::check_action`]).
    pub fn send_action(&self, action: serde_json::Value) -> Result<(), String> {
        const NOT_CONNECTED: &str = "Not connected";
        let streaming = {
            let status = self.status.borrow();
            if !status.connected {
                return Err(NOT_CONNECTED.to_string());
            }
            status.phase != Phase::Idle
        };
        room::check_action(&action, streaming)?;
        self.commands
            .send(Command::Action(action))
            .map_err(|_| NOT_CONNECTED.to_string())
    }

    pub fn status(&self) -> watch::Receiver<Status> {
        self.status.clone()
    }

    /// The load states of the tracks that became current (app only; the CLI has none).
    pub fn tracks(&self) -> watch::Receiver<TrackStates> {
        self.tracks.clone()
    }

    /// Blocks for up to a few seconds; call it from a blocking context.
    pub fn list_apps(&self) -> Result<Vec<AppInfo>, AudioError> {
        self.backend.list_apps()
    }

    /// Stops (restoring the routing), closes the connection and ends all tasks. The track
    /// loader stops its downloads and clears the cache dir before this returns, so a following
    /// [`Self::connect`] never meets a file of this room.
    pub async fn shutdown(self) {
        self.stop().await;
        let _ = self.commands.send(Command::Shutdown);
        if let Some(task) = self.task {
            let _ = task.await;
        }
    }
}

fn store_f64(atomic: &std::sync::atomic::AtomicU64, value: f64) {
    atomic.store(value.to_bits(), Ordering::Relaxed);
}

fn mono_ms() -> f64 {
    mono_ns() as f64 / 1_000_000.0
}

/// From the audio task and the worker to the engine task.
enum Note {
    /// The audio session `session_id` cannot continue.
    Failed { session_id: u64, message: String },
}

struct Channels {
    commands: mpsc::UnboundedReceiver<Command>,
    events: mpsc::UnboundedReceiver<ConnEvent>,
    blocks: mpsc::Receiver<Input>,
    notes: mpsc::UnboundedReceiver<Note>,
}

struct Tasks {
    connection: JoinHandle<()>,
    shutdown_tx: watch::Sender<bool>,
    audio: JoinHandle<()>,
    /// The track loader; ends when the engine task drops its feed.
    loader: Option<JoinHandle<()>>,
}

struct EngineTask {
    engine: Engine,
    /// Dropped at shutdown: `run_connection` returns when it is gone.
    out_tx: Option<mpsc::Sender<Outgoing>>,
    /// Where the raw room messages go; `None` for the CLI, or once the receiver is gone.
    room_feed: Option<mpsc::UnboundedSender<serde_json::Value>>,
    /// The room messages for the track loader; `None` for the CLI, and from shutdown on.
    loader_feed: Option<mpsc::UnboundedSender<serde_json::Value>>,
    audio_tx: Option<mpsc::UnboundedSender<AudioCmd>>,
    status_tx: watch::Sender<Status>,
    shared: Arc<PlayoutShared>,
    /// The last measured mono→epoch offset.
    mono_epoch_offset_ms: f64,
    /// The last epoch→server offset from the engine.
    server_offset_ms: f64,
    offset_known: Arc<AtomicBool>,
    next_session_id: u64,
    /// The audio session that runs; failures of other sessions are stale.
    active_session_id: Option<u64>,
    outgoing_dropped: u64,
}

impl EngineTask {
    async fn run(mut self, mut ch: Channels, tasks: Tasks) {
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut remap = tokio::time::interval(MONO_EPOCH_INTERVAL);
        remap.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                command = ch.commands.recv() => match command {
                    Some(Command::Start { apps, delay_ms }) => {
                        self.feed(Input::UserStart { apps, delay_ms });
                    }
                    Some(Command::UpdateApps { apps }) => self.feed(Input::UserUpdateApps { apps }),
                    Some(Command::SetNudge { nudge_ms }) => {
                        store_f64(&self.shared.nudge_ms, nudge_ms);
                        self.feed(Input::UserNudge { nudge_ms });
                    }
                    // Actions bypass the engine: it has no state for them.
                    Some(Command::Action(action)) => self.send(Outgoing::Action(action)),
                    Some(Command::Stop(done)) => {
                        self.feed(Input::UserStop);
                        self.audio(AudioCmd::Barrier(done));
                    }
                    // A dropped handle shuts down too.
                    Some(Command::Shutdown) | None => break,
                },
                Some(event) = ch.events.recv() => self.on_conn_event(event),
                Some(block) = ch.blocks.recv() => self.feed(block),
                Some(note) = ch.notes.recv() => match note {
                    Note::Failed { session_id, message } => {
                        if self.active_session_id == Some(session_id) {
                            self.feed(Input::AudioFailed { message });
                        }
                    }
                },
                _ = tick.tick() => self.feed(Input::Tick { now_ms: mono_ms() }),
                _ = remap.tick() => self.remap_clock(),
            }
        }
        // The loader stops its downloads and clears the cache while the audio stops.
        self.loader_feed = None;
        self.feed(Input::UserStop);
        // The audio task finishes the queued calls (the stop above included), then ends.
        self.audio_tx = None;
        let _ = tasks.audio.await;
        let out_tx = self.out_tx.take();
        if let Some(out_tx) = &out_tx {
            // Let the queued messages (STOP_LIVE_STREAM) go out before the Close frame.
            let deadline = tokio::time::Instant::now() + DRAIN_TIMEOUT;
            while out_tx.capacity() < out_tx.max_capacity()
                && tokio::time::Instant::now() < deadline
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // The shutdown flag before the sender: a closed queue ends the connection without
        // the Close frame.
        let _ = tasks.shutdown_tx.send(true);
        let _ = tasks.connection.await;
        drop(out_tx);
        if let Some(loader) = tasks.loader {
            let _ = loader.await;
        }
    }

    /// Measures the mono→epoch offset again. On a wall-clock step the engine moves the server
    /// offset by the opposite amount, so server time stays continuous, and resyncs NTP.
    fn remap_clock(&mut self) {
        let offset_ms = MonoEpochMap::measure().to_epoch_ms(0);
        let step = clock_step_ms(self.mono_epoch_offset_ms, offset_ms);
        self.mono_epoch_offset_ms = offset_ms;
        if let Some(delta_ms) = step {
            eprintln!("streamer: the system clock stepped by {delta_ms:.1} ms");
            // Its `ServerOffset` publishes the new epoch part together with the moved server
            // part, never the new epoch part alone.
            self.feed(Input::ClockStepped { delta_ms });
        }
        self.publish_offset();
    }

    /// Stores the mono→server offset for the audio threads as one value. This task is its
    /// only writer.
    fn publish_offset(&self) {
        store_f64(
            &self.shared.mono_server_offset_ms,
            self.mono_epoch_offset_ms + self.server_offset_ms,
        );
    }

    fn on_conn_event(&mut self, event: ConnEvent) {
        let input = match event {
            // For the track loader and the window only: the engine gets the parsed copy, if
            // any, as the next event.
            ConnEvent::Room(message) => {
                if let Some(loader) = &self.loader_feed
                    && loader.send(message.clone()).is_err()
                {
                    self.loader_feed = None;
                }
                if let Some(feed) = &self.room_feed
                    && feed.send(message).is_err()
                {
                    self.room_feed = None;
                }
                return;
            }
            ConnEvent::Connected => Input::Connected,
            ConnEvent::Disconnected => Input::Disconnected,
            ConnEvent::ConnectFailed(error) => Input::ConnectFailed { error },
            ConnEvent::Message(message) => Input::Server(message),
            ConnEvent::NtpResponse {
                t0,
                t1,
                t2,
                t3,
                group_id,
                index,
            } => Input::NtpResponse {
                t0,
                t1,
                t2,
                t3,
                group_id,
                index,
            },
        };
        self.feed(input);
    }

    fn feed(&mut self, input: Input) {
        for output in self.engine.handle(input) {
            self.dispatch(output);
        }
    }

    fn dispatch(&mut self, output: Output) {
        match output {
            Output::SendJson(message) => self.send(Outgoing::Json(message)),
            Output::SendBinary(frame) => self.send(Outgoing::Binary(frame)),
            Output::SendProbePair {
                group_id,
                client_rtt,
                client_nudge_ms,
            } => self.send(Outgoing::ProbePair {
                group_id,
                client_rtt,
                client_nudge_ms,
            }),
            Output::Nudge { nudge_ms } => store_f64(&self.shared.nudge_ms, nudge_ms),
            Output::StartAudio { apps, delay_ms } => {
                self.next_session_id += 1;
                let session_id = self.next_session_id;
                self.active_session_id = Some(session_id);
                self.audio(AudioCmd::Start {
                    session_id,
                    apps,
                    delay_ms,
                });
            }
            Output::UpdateAudioApps { apps } => {
                if let Some(session_id) = self.active_session_id {
                    self.audio(AudioCmd::Update { session_id, apps });
                }
            }
            Output::StopAudio => {
                self.active_session_id = None;
                self.audio(AudioCmd::Stop);
            }
            Output::ServerOffset { offset_ms } => {
                self.server_offset_ms = offset_ms;
                self.publish_offset();
                self.offset_known.store(true, Ordering::Release);
            }
            Output::SetPlayoutGain { gain, ramp_s } => {
                self.shared
                    .gain_ramp_s
                    .store(ramp_s.to_bits(), Ordering::Relaxed);
                self.shared.gain.store(gain.to_bits(), Ordering::Relaxed);
            }
            Output::Status(status) => {
                self.status_tx.send_replace(status);
            }
        }
    }

    /// Never waits: a full queue (a stalled socket) drops the message.
    fn send(&mut self, message: Outgoing) {
        let Some(out_tx) = &self.out_tx else { return };
        if out_tx.try_send(message).is_err() {
            self.outgoing_dropped += 1;
            if self.outgoing_dropped % 250 == 1 {
                eprintln!(
                    "streamer: outgoing queue full, {} messages dropped",
                    self.outgoing_dropped
                );
            }
        }
    }

    fn audio(&self, command: AudioCmd) {
        if let Some(audio_tx) = &self.audio_tx {
            let _ = audio_tx.send(command);
        }
    }
}

/// Downloads the tracks that the [`TrackPlanner`] picks into the cache dir and reads their
/// durations (spec C8). Every state change goes to [`StreamerHandle::tracks`].
struct TrackLoader {
    planner: TrackPlanner,
    client: reqwest::Client,
    /// Resolves server paths (demo mode).
    base: Url,
    cache_dir: PathBuf,
    states: watch::Sender<TrackStates>,
    /// The loads that run, by URL.
    loads: HashMap<String, Load>,
    next_load_id: u64,
    done_tx: mpsc::UnboundedSender<LoadDone>,
}

/// The download and the duration probe of one URL.
struct Load {
    id: u64,
    task: JoinHandle<()>,
}

struct LoadDone {
    url: String,
    /// Tells the late result of a forgotten load from that of a new load of the same URL.
    id: u64,
    /// The duration in seconds, or the text the queue shows.
    result: Result<f64, String>,
}

impl TrackLoader {
    /// Clears the cache dir, then loads until `messages` closes. Then it stops the loads,
    /// clears the cache dir again and publishes an empty map.
    fn spawn(
        base: Url,
        cache_dir: PathBuf,
        states: watch::Sender<TrackStates>,
        messages: mpsc::UnboundedReceiver<serde_json::Value>,
    ) -> JoinHandle<()> {
        let (done_tx, done_rx) = mpsc::unbounded_channel();
        let loader = Self {
            planner: TrackPlanner::default(),
            client: client(),
            base,
            cache_dir,
            states,
            loads: HashMap::new(),
            next_load_id: 0,
            done_tx,
        };
        tokio::spawn(loader.run(messages, done_rx))
    }

    async fn run(
        mut self,
        mut messages: mpsc::UnboundedReceiver<serde_json::Value>,
        mut done: mpsc::UnboundedReceiver<LoadDone>,
    ) {
        // Files of an earlier session that did not end cleanly go first.
        log_removal(
            tokio::fs::remove_dir_all(&self.cache_dir).await,
            &self.cache_dir,
        );
        if let Err(e) = tokio::fs::create_dir_all(&self.cache_dir).await {
            eprintln!(
                "streamer: cannot create the track cache {}: {e}",
                self.cache_dir.display()
            );
        }
        loop {
            let actions = tokio::select! {
                message = messages.recv() => match message {
                    Some(message) => self.planner.on_room_message(&message),
                    None => break,
                },
                Some(LoadDone { url, id, result }) = done.recv() => {
                    if self.loads.get(&url).is_none_or(|load| load.id != id) {
                        continue;
                    }
                    self.loads.remove(&url);
                    self.planner.on_done(&url, result)
                }
            };
            self.apply(actions).await;
            self.publish();
        }
        for load in self.loads.values() {
            load.task.abort();
        }
        for (_, load) in self.loads.drain() {
            let _ = load.task.await;
        }
        log_removal(
            tokio::fs::remove_dir_all(&self.cache_dir).await,
            &self.cache_dir,
        );
        self.states.send_replace(TrackStates::new());
    }

    async fn apply(&mut self, actions: Vec<TrackAction>) {
        for action in actions {
            match action {
                TrackAction::Download { url } => self.start(url),
                TrackAction::Forget { url } => self.forget(&url).await,
            }
        }
    }

    fn start(&mut self, url: String) {
        self.next_load_id += 1;
        let id = self.next_load_id;
        let source = resolve_audio_url(&self.base, &url);
        let dest = self.cache_dir.join(track_file_name(&url));
        let client = self.client.clone();
        let done = self.done_tx.clone();
        let task = tokio::spawn({
            let url = url.clone();
            async move {
                let result = load_track(&client, source, &dest, &url).await;
                let _ = done.send(LoadDone { url, id, result });
            }
        });
        self.loads.insert(url, Load { id, task });
    }

    /// Stops the URL's load before its file goes. The file operations of the aborted task run on
    /// the blocking pool and can finish after the abort, so in rare cases a stale file stays
    /// until the next download of that URL or until leave.
    async fn forget(&mut self, url: &str) {
        if let Some(load) = self.loads.remove(url) {
            load.task.abort();
            // Resolves once the task is dropped, and with it the download's part file.
            let _ = load.task.await;
        }
        let path = self.cache_dir.join(track_file_name(url));
        log_removal(tokio::fs::remove_file(&path).await, &path);
    }

    fn publish(&self) {
        let states = self.planner.states();
        self.states.send_if_modified(|published| {
            let changed = published != states;
            if changed {
                published.clone_from(states);
            }
            changed
        });
    }
}

/// Downloads one track to `dest` and reads its duration. The error is the text the queue
/// shows; the cause goes to the log.
async fn load_track(
    client: &reqwest::Client,
    source: Result<Url, HttpError>,
    dest: &Path,
    url: &str,
) -> Result<f64, String> {
    let downloaded = match source {
        Ok(source) => download(client, &source, dest, MAX_TRACK_BYTES).await,
        Err(e) => Err(e),
    };
    if let Err(e) = downloaded {
        eprintln!("streamer: cannot download the track {url}: {e}");
        return Err(e.to_string());
    }
    let path = dest.to_path_buf();
    match tokio::task::spawn_blocking(move || probe_duration(&path)).await {
        Ok(Ok(duration_s)) => Ok(duration_s),
        Ok(Err(e)) => {
            let cause = std::error::Error::source(&e).map(ToString::to_string);
            eprintln!(
                "streamer: cannot read the track {url}: {}",
                cause.unwrap_or_default()
            );
            Err(e.to_string())
        }
        // The demuxer panicked: the same text as a `MediaError`.
        Err(e) => {
            eprintln!("streamer: reading the track {url} failed: {e}");
            Err("Cannot play this track".to_string())
        }
    }
}

/// Logs a failed removal; a path that is not there is fine.
fn log_removal(result: std::io::Result<()>, path: &Path) {
    if let Err(e) = result
        && e.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!("streamer: cannot remove {}: {e}", path.display());
    }
}

enum AudioCmd {
    Start {
        session_id: u64,
        apps: Vec<String>,
        delay_ms: u32,
    },
    Update {
        session_id: u64,
        apps: Vec<String>,
    },
    Stop,
    Barrier(oneshot::Sender<()>),
}

struct AudioContext {
    backend: Arc<dyn AudioBackend>,
    shared: Arc<PlayoutShared>,
    offset_known: Arc<AtomicBool>,
    blocks: mpsc::Sender<Input>,
    notes: mpsc::UnboundedSender<Note>,
}

/// One audio session: its worker thread.
struct Session {
    stop: Arc<AtomicBool>,
    worker: ThreadHandle<WorkerStats>,
}

async fn audio_task(mut commands: mpsc::UnboundedReceiver<AudioCmd>, cx: AudioContext) {
    let mut session: Option<Session> = None;
    while let Some(command) = commands.recv().await {
        match command {
            AudioCmd::Start {
                session_id,
                apps,
                delay_ms,
            } => {
                if session.is_some() {
                    stop_session(&cx, &mut session).await;
                }
                session = start_session(&cx, session_id, apps, delay_ms).await;
            }
            AudioCmd::Update { session_id, apps } => {
                if session.is_some() {
                    let backend = cx.backend.clone();
                    let result = blocking(move || backend.update_apps(apps)).await;
                    if let Err(message) = result {
                        let _ = cx.notes.send(Note::Failed {
                            session_id,
                            message,
                        });
                    }
                }
            }
            AudioCmd::Stop => stop_session(&cx, &mut session).await,
            AudioCmd::Barrier(done) => {
                let _ = done.send(());
            }
        }
    }
    // The engine task has ended: never leave the apps routed into the sink.
    if session.is_some() {
        stop_session(&cx, &mut session).await;
    }
}

/// Runs a backend call on the blocking pool; a panic becomes an error message.
async fn blocking(
    call: impl FnOnce() -> Result<(), AudioError> + Send + 'static,
) -> Result<(), String> {
    match tokio::task::spawn_blocking(call).await {
        Ok(result) => result.map_err(|e| e.to_string()),
        Err(e) => Err(format!("Audio backend call failed: {e}")),
    }
}

/// Creates fresh rings and a worker, then starts the backend. A failure is reported to the
/// engine; the session still exists, so the engine's stop cleans it up.
async fn start_session(
    cx: &AudioContext,
    session_id: u64,
    apps: Vec<String>,
    delay_ms: u32,
) -> Option<Session> {
    let (capture_tx, capture_rx) = rtrb::RingBuffer::<CapturedChunk>::new(CAPTURE_RING);
    let (playout_tx, playout_rx) = rtrb::RingBuffer::<PcmBlock>::new(PLAYOUT_RING);
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let notes = cx.notes.clone();
    tokio::spawn(async move {
        while let Some(event) = events_rx.recv().await {
            let AudioEvent::Failed(message) = event;
            if notes
                .send(Note::Failed {
                    session_id,
                    message,
                })
                .is_err()
            {
                return;
            }
        }
    });
    let stop = Arc::new(AtomicBool::new(false));
    let worker = Worker {
        session_id,
        capture_rx,
        playout_tx,
        shared: cx.shared.clone(),
        offset_known: cx.offset_known.clone(),
        blocks: cx.blocks.clone(),
        notes: cx.notes.clone(),
        delay_ms,
        stop: stop.clone(),
    };
    let worker = match std::thread::Builder::new()
        .name("streamer-audio".to_string())
        .spawn(move || worker.run())
    {
        Ok(handle) => handle,
        Err(e) => {
            let message = format!("Cannot start the audio worker: {e}");
            let _ = cx.notes.send(Note::Failed {
                session_id,
                message,
            });
            return None;
        }
    };

    let io = AudioIo {
        capture_tx,
        playout_rx,
        shared: cx.shared.clone(),
        events: events_tx,
    };
    let backend = cx.backend.clone();
    if let Err(message) = blocking(move || backend.start(apps, io)).await {
        let _ = cx.notes.send(Note::Failed {
            session_id,
            message,
        });
    }
    Some(Session { stop, worker })
}

/// Stops the backend (routing restored when it returns), then the worker.
async fn stop_session(cx: &AudioContext, session: &mut Option<Session>) {
    let backend = cx.backend.clone();
    if let Err(message) = blocking(move || backend.stop()).await {
        eprintln!("streamer: stopping the audio failed: {message}");
    }
    let Some(Session { stop, worker }) = session.take() else {
        return;
    };
    stop.store(true, Ordering::Relaxed);
    if let Ok(Ok(stats)) = tokio::task::spawn_blocking(move || worker.join()).await {
        let capture_dropped = cx.shared.capture_dropped.load(Ordering::Relaxed);
        eprintln!(
            "streamer: audio stopped: {} blocks, {} local-copy drops, {} send drops, \
             {} chunks before the first server offset, {} capture drops in total",
            stats.blocks,
            stats.playout_dropped,
            stats.send_dropped,
            stats.no_offset_chunks,
            capture_dropped,
        );
    }
}

#[derive(Debug, Default)]
struct WorkerStats {
    blocks: u64,
    playout_dropped: u64,
    send_dropped: u64,
    no_offset_chunks: u64,
}

/// The audio worker of one session (a std thread: it must not wait on the async runtime).
struct Worker {
    session_id: u64,
    capture_rx: rtrb::Consumer<CapturedChunk>,
    playout_tx: rtrb::Producer<PcmBlock>,
    shared: Arc<PlayoutShared>,
    offset_known: Arc<AtomicBool>,
    blocks: mpsc::Sender<Input>,
    notes: mpsc::UnboundedSender<Note>,
    delay_ms: u32,
    stop: Arc<AtomicBool>,
}

impl Worker {
    fn run(mut self) -> WorkerStats {
        let mut stats = WorkerStats::default();
        let mut encoder = match OpusFrameEncoder::new(CHANNELS as u8) {
            Ok(encoder) => encoder,
            Err(e) => {
                let message = format!("Cannot create the Opus encoder: {e}");
                let _ = self.notes.send(Note::Failed {
                    session_id: self.session_id,
                    message,
                });
                return stats;
            }
        };
        let lookahead = encoder.lookahead_samples();
        let mut timeline =
            CaptureTimeline::new(FRAME_DURATION_MS, TIMELINE_ALPHA, TIMELINE_REANCHOR_MS);
        let mut assembler = BlockAssembler::new();
        while !self.stop.load(Ordering::Relaxed) {
            let Ok(chunk) = self.capture_rx.pop() else {
                std::thread::sleep(WORKER_IDLE);
                continue;
            };
            // Stamps need the server offset; until the first one, nothing is stamped.
            if !self.offset_known.load(Ordering::Acquire) {
                assembler.clear();
                stats.no_offset_chunks += 1;
                continue;
            }
            if assembler.is_gap(&chunk) {
                assembler.clear();
                timeline.reset();
            }
            assembler.push(&chunk, |first_ns, samples| {
                stats.blocks += 1;
                let stamp = timeline.stamp(self.shared.mono_to_server_ms(first_ns));
                match encoder.encode(samples) {
                    Ok(payload) => {
                        let block = Input::EncodedBlock {
                            capture_server_ms: stamp,
                            payload,
                        };
                        if self.blocks.try_send(block).is_err() {
                            stats.send_dropped += 1;
                        }
                    }
                    Err(e) => eprintln!("streamer: cannot encode a block: {e}"),
                }
                let block = PcmBlock {
                    target_start_server_ms: local_target_ms(
                        stamp,
                        self.delay_ms,
                        lookahead,
                        self.shared.nudge_ms(),
                    ),
                    samples: *samples,
                };
                if self.playout_tx.push(block).is_err() {
                    stats.playout_dropped += 1;
                }
            });
        }
        stats
    }
}

fn frames_to_ns(frames: usize) -> i64 {
    (frames as f64 * 1e9 / f64::from(SAMPLE_RATE)).round() as i64
}

/// Cuts captured chunks into 960-frame blocks and gives the CLOCK_MONOTONIC time of each
/// block's first frame.
struct BlockAssembler {
    samples: [f32; BLOCK_SAMPLES],
    /// Frames in `samples`.
    filled: usize,
    first_ns: i64,
    /// Where the next chunk should start; `None` before the first chunk.
    next_ns: Option<i64>,
}

impl BlockAssembler {
    fn new() -> Self {
        Self {
            samples: [0.0; BLOCK_SAMPLES],
            filled: 0,
            first_ns: 0,
            next_ns: None,
        }
    }

    /// The chunk does not continue the previous one: too much audio was lost before it, or
    /// its time does not follow the previous chunk and the lost frames (a capture restart).
    fn is_gap(&self, chunk: &CapturedChunk) -> bool {
        frames_to_ns(chunk.lost_frames) > MAX_SILENT_FILL_NS
            || self.next_ns.is_some_and(|next| {
                let expected_ns = next + frames_to_ns(chunk.lost_frames);
                (chunk.mono_ns_first_frame - expected_ns).abs() > CAPTURE_GAP_NS
            })
    }

    /// Forgets the partial block and the expected next chunk.
    fn clear(&mut self) {
        self.filled = 0;
        self.next_ns = None;
    }

    /// Adds the chunk. If it continues the previous chunk, silence comes first in place of the
    /// lost frames: the timestamps stay continuous, and every device plays the same silence.
    /// Check [`Self::is_gap`] first.
    fn push(&mut self, chunk: &CapturedChunk, mut emit: impl FnMut(i64, &[f32; BLOCK_SAMPLES])) {
        if let Some(next) = self.next_ns {
            self.append(next, None, chunk.lost_frames, &mut emit);
        }
        let frames = chunk.frames.min(CHUNK_MAX_FRAMES);
        let samples = &chunk.samples[..frames * CHANNELS];
        self.append(chunk.mono_ns_first_frame, Some(samples), frames, &mut emit);
        self.next_ns = Some(chunk.mono_ns_first_frame + frames_to_ns(frames));
    }

    /// Adds `frames` frames that start at `start_ns`: from `samples`, or silence for `None`.
    fn append(
        &mut self,
        start_ns: i64,
        samples: Option<&[f32]>,
        frames: usize,
        emit: &mut impl FnMut(i64, &[f32; BLOCK_SAMPLES]),
    ) {
        let mut index = 0;
        while index < frames {
            if self.filled == 0 {
                self.first_ns = start_ns + frames_to_ns(index);
            }
            let take = (FRAME_SAMPLES - self.filled).min(frames - index);
            let to = &mut self.samples[self.filled * CHANNELS..(self.filled + take) * CHANNELS];
            match samples {
                Some(from) => {
                    to.copy_from_slice(&from[index * CHANNELS..(index + take) * CHANNELS]);
                }
                None => to.fill(0.0),
            }
            self.filled += take;
            index += take;
            if self.filled == FRAME_SAMPLES {
                emit(self.first_ns, &self.samples);
                self.filled = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::test_server::{Reply, TestServer};
    use crate::test_util::TempDir;
    use crate::tracks::{TrackState, track_file_name};
    use futures_util::SinkExt;
    use serde_json::{Value, json};
    use tokio_tungstenite::tungstenite::Message;

    #[test]
    fn local_target_includes_lookahead_and_nudge() {
        // Without the lookahead the laptop is 6.5 ms ahead of every phone.
        assert_eq!(local_target_ms(1000.0, 1000, 312, 0.0), 2006.5);
        // nudge -40 = 40 ms later (web model)
        assert_eq!(local_target_ms(1000.0, 1000, 312, -40.0), 2046.5);
    }

    /// A chunk of `frames` stereo frames whose samples count up from `first_frame`.
    fn chunk(mono_ns: i64, first_frame: usize, frames: usize) -> CapturedChunk {
        let mut c = CapturedChunk {
            mono_ns_first_frame: mono_ns,
            frames,
            samples: [0.0; CHUNK_MAX_FRAMES * 2],
            lost_frames: 0,
        };
        for i in 0..frames {
            let v = (first_frame + i) as f32;
            c.samples[2 * i] = v;
            c.samples[2 * i + 1] = -v;
        }
        c
    }

    #[test]
    fn blocks_carry_their_first_frame_time_across_chunks() {
        // Stamping a block with its chunk's time instead of its own first frame would add up
        // to 21 ms of jitter that the timeline only partly hides.
        let mut asm = BlockAssembler::new();
        let t0 = 7_000_000_000_i64;
        let mut blocks: Vec<(i64, f32, f32)> = Vec::new();
        for n in 0..15 {
            let c = chunk(t0 + frames_to_ns(n * 1024), n * 1024, 1024);
            assert!(!asm.is_gap(&c));
            asm.push(&c, |first_ns, s| {
                blocks.push((first_ns, s[0], s[BLOCK_SAMPLES - 1]))
            });
        }
        assert_eq!(blocks.len(), 15 * 1024 / FRAME_SAMPLES);
        for (k, &(first_ns, first, last)) in blocks.iter().enumerate() {
            let expected_ns = t0 + frames_to_ns(k * FRAME_SAMPLES);
            assert!((first_ns - expected_ns).abs() <= 1, "block {k}");
            assert_eq!(
                first,
                (k * FRAME_SAMPLES) as f32,
                "block {k} starts in order"
            );
            assert_eq!(
                last,
                -(((k + 1) * FRAME_SAMPLES - 1) as f32),
                "block {k} ends"
            );
        }

        // A 100 ms jump with no lost frames counted (a capture restart): the partial block is
        // dropped, the next block starts after it.
        let resume_ns = t0 + frames_to_ns(15 * 1024) + 100_000_000;
        let c = chunk(resume_ns, 0, 1024);
        assert!(asm.is_gap(&c));
        asm.clear();
        blocks.clear();
        asm.push(&c, |first_ns, s| blocks.push((first_ns, s[0], 0.0)));
        assert_eq!(blocks, vec![(resume_ns, 0.0, 0.0)]);
    }

    #[test]
    fn a_long_loss_is_a_gap() {
        // Filled, a long loss goes out as a burst of silent blocks. A restart steps the
        // timestamps by far more than 50 ms, so every device resyncs.
        let mut asm = BlockAssembler::new();
        let t0 = 7_000_000_000_i64;
        asm.push(&chunk(t0, 0, 1024), |_, _| {});
        // 213 ms lost, and the time of the next chunk agrees.
        let mut resumed = chunk(t0 + frames_to_ns(11 * 1024), 11 * 1024, 1024);
        resumed.lost_frames = 10 * 1024;
        assert!(asm.is_gap(&resumed));
    }

    #[test]
    fn lost_audio_becomes_silence_in_time() {
        // The web receiver plays consecutive sequence numbers back to back through any
        // timestamp step below 50 ms, but the local copy resyncs on a step above 0.5 ms. So a
        // short loss must not step the timestamps: both play the lost frames as silence.
        let mut asm = BlockAssembler::new();
        let t0 = 7_000_000_000_i64;
        // Frames 1024..4096 (64 ms) are lost, more than the 50 ms time check.
        let mut resumed = chunk(t0 + frames_to_ns(4 * 1024), 4 * 1024, 1024);
        resumed.lost_frames = 3 * 1024;
        let chunks = [
            chunk(t0, 0, 1024),
            resumed,
            chunk(t0 + frames_to_ns(5 * 1024), 5 * 1024, 1024),
        ];
        let mut blocks: Vec<i64> = Vec::new();
        let mut left: Vec<f32> = Vec::new();
        for c in &chunks {
            assert!(!asm.is_gap(c), "chunk at frame {}", c.samples[0]);
            asm.push(c, |first_ns, s| {
                blocks.push(first_ns);
                left.extend(s.iter().step_by(CHANNELS));
            });
        }
        assert_eq!(blocks.len(), 6 * 1024 / FRAME_SAMPLES);
        for (k, &first_ns) in blocks.iter().enumerate() {
            let expected_ns = t0 + frames_to_ns(k * FRAME_SAMPLES);
            assert!((first_ns - expected_ns).abs() <= 1, "block {k}");
        }
        for (k, &v) in left.iter().enumerate() {
            let want = if (1024..4 * 1024).contains(&k) {
                0.0
            } else {
                k as f32
            };
            assert_eq!(v, want, "frame {k}");
        }
    }

    /// Records the backend calls; `start` succeeds and keeps the audio ends like a real
    /// backend.
    #[derive(Default)]
    struct FakeBackend {
        calls: std::sync::Mutex<Vec<&'static str>>,
        io: std::sync::Mutex<Option<AudioIo>>,
    }

    impl FakeBackend {
        fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().unwrap().clone()
        }

        fn record(&self, call: &'static str) {
            self.calls.lock().unwrap().push(call);
        }
    }

    impl AudioBackend for FakeBackend {
        fn list_apps(&self) -> Result<Vec<AppInfo>, AudioError> {
            Ok(Vec::new())
        }

        fn start(&self, _apps: Vec<String>, io: AudioIo) -> Result<(), AudioError> {
            self.record("start");
            *self.io.lock().unwrap() = Some(io);
            Ok(())
        }

        fn update_apps(&self, _apps: Vec<String>) -> Result<(), AudioError> {
            self.record("update_apps");
            Ok(())
        }

        fn stop(&self) -> Result<(), AudioError> {
            self.record("stop");
            self.io.lock().unwrap().take();
            Ok(())
        }
    }

    const WAIT: Duration = Duration::from_secs(4);

    fn settings(server_url: String) -> Settings {
        Settings {
            server_url,
            room_id: "123456".to_string(),
            username: "alice".to_string(),
            client_id: "me".to_string(),
            ..Settings::default()
        }
    }

    /// A local WebSocket server and the settings to join it.
    async fn server() -> (tokio::net::TcpListener, Settings) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/ws", listener.local_addr().unwrap());
        (listener, settings(url))
    }

    type Ws = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

    async fn accept(listener: &tokio::net::TcpListener) -> Ws {
        let (socket, _) = tokio::time::timeout(WAIT, listener.accept())
            .await
            .expect("the runtime connects")
            .unwrap();
        tokio_tungstenite::accept_async(socket).await.unwrap()
    }

    /// Polls `done` every few ms until it is true; panics after [`WAIT`].
    async fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let poll = async {
            while !done() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(WAIT, poll)
            .await
            .unwrap_or_else(|_| panic!("timed out waiting until {what}"));
    }

    /// [`StreamerHandle::connect`] for the app with the track cache in `cache`: the loader
    /// clears its cache dir, and no test may touch the user's.
    async fn connect_app(
        settings: Settings,
        backend: Arc<FakeBackend>,
        room_feed: Option<mpsc::UnboundedSender<Value>>,
        cache: &TempDir,
    ) -> StreamerHandle {
        StreamerHandle::connect_in(settings, backend, JoinKind::App, room_feed, cache.0.clone())
            .await
    }

    #[tokio::test]
    async fn room_feed_receives_room_messages() {
        let (listener, settings) = server().await;
        let (feed_tx, mut feed_rx) = mpsc::unbounded_channel();
        let backend = Arc::new(FakeBackend::default());
        let cache = TempDir::new("runtime-feed");
        let handle = connect_app(settings, backend, Some(feed_tx), &cache).await;
        let mut ws = accept(&listener).await;
        let text = r#"{"type":"ROOM_EVENT","event":{"type":"CLIENT_CHANGE","clients":[{"username":"bob","clientId":"b","isAdmin":true,"joinedAt":1}]}}"#;
        ws.send(tokio_tungstenite::tungstenite::Message::text(text))
            .await
            .unwrap();
        let message = tokio::time::timeout(WAIT, feed_rx.recv())
            .await
            .expect("the feed gets the message")
            .unwrap();
        assert_eq!(
            message,
            serde_json::from_str::<serde_json::Value>(text).unwrap()
        );
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn actions_are_refused_while_disconnected() {
        // A chat message or an admin change typed while the socket is down must show an
        // error, not vanish.
        let (listener, settings) = server().await;
        drop(listener);
        let backend = Arc::new(FakeBackend::default());
        let cache = TempDir::new("runtime-refused");
        let handle = connect_app(settings, backend, None, &cache).await;
        let chat = serde_json::json!({ "type": "SEND_CHAT_MESSAGE", "text": "hi" });
        assert_eq!(handle.send_action(chat), Err("Not connected".to_string()));
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn a_failed_connect_shows_the_reason_in_the_status() {
        // A wrong but parsable server address: the sync screen must say why it waits.
        let (listener, settings) = server().await;
        drop(listener);
        let backend = Arc::new(FakeBackend::default());
        let cache = TempDir::new("runtime-connect-error");
        let handle = connect_app(settings, backend, None, &cache).await;
        let status = handle.status();
        wait_until("the status has the connect error", || {
            status
                .borrow()
                .message
                .as_deref()
                .is_some_and(|m| m.starts_with("Cannot reach the server: "))
        })
        .await;
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_stops_the_audio_before_returning() {
        // Leave while this PC streams: the join screen shows when `shutdown` returns, and the
        // apps must not stay routed into the sink.
        let (listener, settings) = server().await;
        let backend = Arc::new(FakeBackend::default());
        let cache = TempDir::new("runtime-shutdown");
        let handle = connect_app(settings, backend.clone(), None, &cache).await;
        let _ws = accept(&listener).await;
        handle.start(vec!["app".to_string()], 1000);
        wait_until("the backend starts", || backend.calls().contains(&"start")).await;
        handle.shutdown().await;
        assert_eq!(backend.calls(), ["start", "stop"]);
    }

    async fn send(ws: &mut Ws, message: Value) {
        ws.send(Message::text(message.to_string())).await.unwrap();
    }

    fn queue(urls: &[&str]) -> Value {
        let sources: Vec<Value> = urls.iter().map(|url| json!({ "url": url })).collect();
        json!({
            "type": "ROOM_EVENT",
            "event": { "type": "SET_AUDIO_SOURCES", "sources": sources },
        })
    }

    fn play(url: &str) -> Value {
        json!({
            "type": "SCHEDULED_ACTION",
            "serverTimeToExecute": 1000,
            "scheduledAction": { "type": "PLAY", "audioSource": url, "trackTimeSeconds": 0 },
        })
    }

    /// Serves the 2 s tone at /tone.mp3, an HTML error page at /page.mp3 and, at
    /// /stalled.mp3, a body that stops halfway; anything else is a 404.
    async fn track_server() -> TestServer {
        let tone = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/media/tone.mp3"),
        )
        .unwrap();
        TestServer::start(move |request| match request.target.as_str() {
            "/tone.mp3" => Reply::bytes(tone.clone()),
            "/page.mp3" => Reply::bytes(b"<!DOCTYPE html><html><body>Oops</body></html>".to_vec()),
            "/stalled.mp3" => Reply::bytes(vec![0; 100_000]).stalled_after(50_000),
            _ => Reply::text(404, "Not found"),
        })
        .await
    }

    /// The app joined to a fresh local room server, with its track cache in `cache`.
    async fn join_app(cache: &TempDir) -> (StreamerHandle, Ws, tokio::net::TcpListener) {
        let (listener, settings) = server().await;
        let backend = Arc::new(FakeBackend::default());
        let handle = connect_app(settings, backend, None, cache).await;
        let ws = accept(&listener).await;
        (handle, ws, listener)
    }

    fn state(tracks: &watch::Receiver<TrackStates>, url: &str) -> Option<TrackState> {
        tracks.borrow().get(url).cloned()
    }

    fn is_ready(tracks: &watch::Receiver<TrackStates>, url: &str) -> bool {
        matches!(state(tracks, url), Some(TrackState::Ready { .. }))
    }

    /// Plays the tone and the stalled track; returns when the tone is ready and the stalled
    /// download has written part of its body.
    async fn play_tone_and_stalled(
        ws: &mut Ws,
        tracks: &watch::Receiver<TrackStates>,
        cache: &TempDir,
        tone: &str,
        stalled: &str,
    ) {
        send(ws, queue(&[tone, stalled])).await;
        send(ws, play(tone)).await;
        send(ws, play(stalled)).await;
        let part = cache.0.join(format!("{}.part", track_file_name(stalled)));
        wait_until(
            "the tone is ready and the stalled download has data",
            || is_ready(tracks, tone) && std::fs::metadata(&part).is_ok_and(|m| m.len() > 0),
        )
        .await;
        assert_eq!(state(tracks, stalled), Some(TrackState::Loading));
    }

    #[tokio::test]
    async fn a_played_track_becomes_ready_with_its_duration() {
        let http = track_server().await;
        let url = http.url("/tone.mp3").to_string();
        let cache = TempDir::new("tracks-ready");
        let (handle, mut ws, _listener) = join_app(&cache).await;
        let tracks = handle.tracks();

        send(&mut ws, queue(&[&url])).await;
        send(&mut ws, play(&url)).await;
        wait_until("the track is ready", || is_ready(&tracks, &url)).await;

        let Some(TrackState::Ready { duration_s }) = state(&tracks, &url) else {
            unreachable!()
        };
        assert!((duration_s - 2.0).abs() <= 0.06, "{duration_s}");
        // The file stays while the track is in the queue (part 3 plays it).
        assert_eq!(cache.files(), [track_file_name(&url)]);
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn a_failed_download_marks_the_track_error() {
        // Review Focus 1: the queue shows why the track does not load, and nothing hangs.
        let http = track_server().await;
        let missing = http.url("/missing.mp3").to_string();
        let page = http.url("/page.mp3").to_string();
        let cache = TempDir::new("tracks-error");
        let (handle, mut ws, _listener) = join_app(&cache).await;
        let tracks = handle.tracks();

        send(&mut ws, queue(&[&missing, &page])).await;
        send(&mut ws, play(&missing)).await;
        send(&mut ws, play(&page)).await;
        wait_until("both tracks are errors", || {
            let states = tracks.borrow();
            [&missing, &page]
                .iter()
                .all(|url| matches!(states.get(*url), Some(TrackState::Error { .. })))
        })
        .await;

        let error = |message: &str| {
            Some(TrackState::Error {
                message: message.to_string(),
            })
        };
        assert_eq!(
            state(&tracks, &missing),
            error("Download failed: 404 Not Found")
        );
        assert_eq!(state(&tracks, &page), error("Cannot play this track"));
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn a_track_removed_from_the_queue_is_deleted() {
        // A deleted track must not keep its file, nor its unfinished download a part file.
        let http = track_server().await;
        let tone = http.url("/tone.mp3").to_string();
        let stalled = http.url("/stalled.mp3").to_string();
        let cache = TempDir::new("tracks-forget");
        let (handle, mut ws, _listener) = join_app(&cache).await;
        let tracks = handle.tracks();
        play_tone_and_stalled(&mut ws, &tracks, &cache, &tone, &stalled).await;

        send(&mut ws, queue(&[])).await;
        wait_until("the tracks and their files are gone", || {
            tracks.borrow().is_empty() && cache.files().is_empty()
        })
        .await;
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn leave_clears_the_tracks_and_the_cache() {
        // Review Focus 2: after leave, no track state, download or file of the old room is
        // left, and the next join starts clean.
        let http = track_server().await;
        let tone = http.url("/tone.mp3").to_string();
        let stalled = http.url("/stalled.mp3").to_string();
        let cache = TempDir::new("tracks-leave");
        let (handle, mut ws, listener) = join_app(&cache).await;
        let tracks = handle.tracks();
        play_tone_and_stalled(&mut ws, &tracks, &cache, &tone, &stalled).await;

        handle.shutdown().await;
        assert_eq!(cache.files(), Vec::<String>::new());
        assert!(tracks.borrow().is_empty());

        // A file left by a crash goes when the next join starts.
        std::fs::create_dir_all(&cache.0).unwrap();
        std::fs::write(cache.0.join("old.mp3"), b"old").unwrap();
        let backend = Arc::new(FakeBackend::default());
        let settings = settings(format!("ws://{}/ws", listener.local_addr().unwrap()));
        let handle = connect_app(settings, backend, None, &cache).await;
        let _ws = accept(&listener).await;
        assert!(handle.tracks().borrow().is_empty());
        wait_until("the old file is gone", || cache.files().is_empty()).await;
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn the_cli_downloads_nothing() {
        // The CLI shares the cache dir with the app: it neither downloads into it nor clears
        // it.
        let http = track_server().await;
        let url = http.url("/tone.mp3").to_string();
        let cache = TempDir::new("tracks-cli");
        std::fs::write(cache.0.join("app-track.mp3"), b"x").unwrap();
        let (listener, settings) = server().await;
        let (feed_tx, mut feed_rx) = mpsc::unbounded_channel();
        let backend = Arc::new(FakeBackend::default());
        let handle = StreamerHandle::connect_in(
            settings,
            backend,
            JoinKind::Streamer,
            Some(feed_tx),
            cache.0.clone(),
        )
        .await;
        let mut ws = accept(&listener).await;

        send(&mut ws, queue(&[&url])).await;
        send(&mut ws, play(&url)).await;
        for _ in 0..2 {
            tokio::time::timeout(WAIT, feed_rx.recv())
                .await
                .expect("the core gets the message");
        }
        // The core has the PLAY; give a loader time to act on it.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(http.requests().is_empty(), "{:?}", http.requests());
        assert!(handle.tracks().borrow().is_empty());
        handle.shutdown().await;
        assert_eq!(cache.files(), ["app-track.mp3"]);
    }
}
