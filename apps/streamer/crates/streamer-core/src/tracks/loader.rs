//! The track loader: downloads the tracks that the [`TrackPlanner`] picks into the cache dir,
//! decodes each into a decoded-track file next to its download, and acknowledges the load
//! requests of the server. It also plays the queue on this PC: the [`QueuePlayer`] gets the
//! room messages, the load results and a 10 ms tick, and its audio actions go to the feeder
//! thread. The engine task feeds it the room messages.

use std::collections::HashMap;
use std::io;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use url::Url;

use super::feeder::{FeedCmd, spawn_feeder};
use super::{
    PlayerAction, QueuePlayer, TrackAction, TrackPlanner, TrackStates, decoded_file_name,
    track_file_name,
};
use crate::audio::PlayoutShared;
use crate::clock::mono_ns;
use crate::connection::Outgoing;
use crate::http::{HttpError, client, download, resolve_audio_url};
use crate::media::{MediaError, PcmFile, PcmFileWriter, PcmSink, decode_to};
use crate::playout::PcmBlock;
use crate::protocol::{AudioSource, ClientMessage};

/// K4: a larger track is not downloaded; it shows "Track is larger than 200 MB".
const MAX_TRACK_BYTES: u64 = 200_000_000;
/// How often the queue player checks for the end of the playing track.
const TICK: Duration = Duration::from_millis(10);

/// What the loader needs to play the queue on this PC.
pub(crate) struct Playback {
    /// This client's id, to know whether it is an admin.
    pub(crate) client_id: String,
    /// The producer of the output's track slot.
    pub(crate) track_tx: rtrb::Producer<PcmBlock>,
    pub(crate) shared: Arc<PlayoutShared>,
    /// True once the server offset is known.
    pub(crate) offset_known: Arc<AtomicBool>,
    /// The shuffle state of the window.
    pub(crate) shuffle: Arc<AtomicBool>,
}

/// Downloads and decodes the tracks that the [`TrackPlanner`] picks (spec C8), keeps the
/// decoded files that it keeps, and sends its acks to the server. Every state change goes to
/// [`crate::runtime::StreamerHandle::tracks`]. Plays the queue track through the feeder.
pub(crate) struct TrackLoader {
    planner: TrackPlanner,
    client: reqwest::Client,
    /// Resolves server paths (demo mode).
    base: Url,
    cache_dir: PathBuf,
    states: watch::Sender<TrackStates>,
    /// The loads that run, by URL.
    loads: HashMap<String, Load>,
    /// The decoded tracks, by URL: the ones the planner keeps.
    decoded: HashMap<String, Arc<PcmFile>>,
    next_load_id: u64,
    done_tx: mpsc::UnboundedSender<LoadDone>,
    /// The engine task's queue to the server, for the acks and the next track's `PLAY`.
    out: mpsc::Sender<Outgoing>,
    /// A message was dropped already; only the first drop is logged.
    out_dropped: bool,
    player: QueuePlayer,
    /// The feeder's commands; `None` without a feeder thread, and from shutdown on.
    feeder_tx: Option<std::sync::mpsc::Sender<FeedCmd>>,
    feeder: Option<std::thread::JoinHandle<()>>,
    shared: Arc<PlayoutShared>,
    offset_known: Arc<AtomicBool>,
    shuffle: Arc<AtomicBool>,
    rand: XorShift,
}

/// The download or the decode of one URL.
struct Load {
    id: u64,
    /// The download, an async task that an abort stops, or the decode, a blocking task that
    /// stops before its next packet once `cancel` is set.
    task: JoinHandle<()>,
    cancel: Arc<AtomicBool>,
}

impl Load {
    fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        // A blocking task that runs ignores the abort; one that has not started never runs.
        self.task.abort();
    }

    /// Stops the load and waits until it no longer touches its files: the dropped download
    /// removes its part file, the stopped decode its own.
    async fn stop(self) {
        self.cancel();
        let _ = self.task.await;
    }
}

/// What wakes the loader up.
enum Event {
    Message(serde_json::Value),
    Done(LoadDone),
    Tick,
}

enum Step {
    /// The download ended; on success the decode follows.
    Downloaded(Result<(), String>),
    /// The decode ended: the decoded track, or the text the queue shows.
    Decoded(Result<PcmFile, String>),
}

struct LoadDone {
    url: String,
    /// Tells the late result of a forgotten load from that of a new load of the same URL.
    id: u64,
    step: Step,
}

impl TrackLoader {
    /// Clears the cache dir, then loads and plays until `messages` closes. Then it stops the
    /// feeder (before the engine task closes the output), stops the loads, clears the cache
    /// dir again and publishes an empty map. The acks and the next track's `PLAY` go to `out`.
    pub(crate) fn spawn(
        base: Url,
        cache_dir: PathBuf,
        states: watch::Sender<TrackStates>,
        messages: mpsc::UnboundedReceiver<serde_json::Value>,
        out: mpsc::Sender<Outgoing>,
        playback: Playback,
    ) -> JoinHandle<()> {
        let (done_tx, done_rx) = mpsc::unbounded_channel();
        let (feeder_tx, feeder_rx) = std::sync::mpsc::channel();
        let feeder = spawn_feeder(
            playback.track_tx,
            playback.shared.clone(),
            playback.offset_known.clone(),
            feeder_rx,
        )
        .inspect_err(|e| {
            eprintln!("streamer: cannot start the track feeder, no queue playback: {e}")
        })
        .ok();
        let loader = Self {
            planner: TrackPlanner::default(),
            client: client(),
            base,
            cache_dir,
            states,
            loads: HashMap::new(),
            decoded: HashMap::new(),
            next_load_id: 0,
            done_tx,
            out,
            out_dropped: false,
            player: QueuePlayer::new(playback.client_id),
            feeder_tx: feeder.is_some().then_some(feeder_tx),
            feeder,
            shared: playback.shared,
            offset_known: playback.offset_known,
            shuffle: playback.shuffle,
            rand: XorShift::from_clock(),
        };
        tokio::spawn(loader.run(messages, done_rx))
    }

    /// The decoded track of the URL, while the planner keeps its file. A reader keeps the
    /// data of an evicted track until it drops the file.
    pub(crate) fn decoded(&self, url: &str) -> Option<Arc<PcmFile>> {
        self.decoded.get(url).cloned()
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
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            let event = tokio::select! {
                message = messages.recv() => match message {
                    Some(message) => Event::Message(message),
                    None => break,
                },
                Some(load_done) = done.recv() => Event::Done(load_done),
                _ = tick.tick() => Event::Tick,
            };
            match event {
                Event::Message(message) => {
                    // The planner first, so that a PLAY of a track that is not decoded starts
                    // its load.
                    let actions = self.planner.on_room_message(&message);
                    self.apply(actions).await;
                    let actions = self.player.on_room_message(&message);
                    self.play(actions);
                }
                Event::Done(LoadDone { url, id, step }) => {
                    if self.loads.get(&url).is_none_or(|load| load.id != id) {
                        continue;
                    }
                    let Some(result) = self.next_step(&url, step) else {
                        continue;
                    };
                    let actions = self.planner.on_done(&url, result.clone());
                    self.apply(actions).await;
                    match result {
                        Ok(duration_s) => {
                            let now = self.server_now_ms();
                            let actions = self.player.on_track_ready(&url, duration_s, now);
                            self.play(actions);
                        }
                        Err(_) => self.player.on_track_failed(&url),
                    }
                }
                Event::Tick => {
                    self.tick();
                    continue;
                }
            }
            self.publish();
        }
        self.stop_feeder().await;
        for load in self.loads.values() {
            load.cancel();
        }
        for (_, load) in self.loads.drain() {
            let _ = load.task.await;
        }
        self.decoded.clear();
        log_removal(
            tokio::fs::remove_dir_all(&self.cache_dir).await,
            &self.cache_dir,
        );
        self.states.send_replace(TrackStates::new());
    }

    /// Goes on with a load after one of its steps: a finished download starts the decode
    /// (`None`); anything else ends the load and returns its result for the planner: the
    /// duration of the decoded track, or the error text.
    fn next_step(&mut self, url: &str, step: Step) -> Option<Result<f64, String>> {
        let result = match step {
            Step::Downloaded(Ok(())) => {
                let load = &self.loads[url];
                let (id, cancel) = (load.id, load.cancel.clone());
                self.start_decode(url.to_string(), id, cancel);
                return None;
            }
            Step::Downloaded(Err(message)) | Step::Decoded(Err(message)) => Err(message),
            Step::Decoded(Ok(pcm)) => {
                let duration_s = pcm.duration_s();
                self.decoded.insert(url.to_string(), Arc::new(pcm));
                Ok(duration_s)
            }
        };
        self.loads.remove(url);
        Some(result)
    }

    /// The server time now. Before the server offset is known it is this PC's clock.
    fn server_now_ms(&self) -> f64 {
        self.shared.mono_to_server_ms(mono_ns())
    }

    /// Lets the player look for the end of the playing track, once the server offset is
    /// known.
    fn tick(&mut self) {
        if !self.offset_known.load(Ordering::Acquire) {
            return;
        }
        let now = self.server_now_ms();
        let shuffle = self.shuffle.load(Ordering::Relaxed);
        let rand = &mut self.rand;
        let actions = self.player.tick(now, shuffle, &mut || rand.next_f64());
        self.play(actions);
    }

    /// Carries out the player's actions: the audio ones go to the feeder, the next track's
    /// `PLAY` to the server.
    fn play(&mut self, actions: Vec<PlayerAction>) {
        for action in actions {
            let cmd = match action {
                PlayerAction::Play {
                    url,
                    start_server_ms,
                    position_s,
                } => match self.decoded(&url) {
                    Some(pcm) => FeedCmd::Play {
                        pcm,
                        start_server_ms,
                        position_s,
                    },
                    None => {
                        // The decoded file went first: the track starts at the same anchor
                        // once its decode (which the PLAY started) ends.
                        self.player.on_play_unavailable(&url);
                        FeedCmd::StopNow
                    }
                },
                PlayerAction::StopAt { server_ms } => FeedCmd::StopAt { server_ms },
                PlayerAction::StopNow => FeedCmd::StopNow,
                PlayerAction::SendPlay { url } => {
                    let play = serde_json::json!({
                        "type": "PLAY",
                        "trackTimeSeconds": 0,
                        "audioSource": url,
                    });
                    self.send(Outgoing::Action(play));
                    continue;
                }
            };
            if let Some(feeder) = &self.feeder_tx
                && feeder.send(cmd).is_err()
            {
                // The feeder thread is gone (it panicked): this PC stays silent.
                self.feeder_tx = None;
            }
        }
    }

    /// Drops the feeder's commands and waits until its thread has ended; a track that is
    /// still queued fades out first.
    async fn stop_feeder(&mut self) {
        self.feeder_tx = None;
        if let Some(feeder) = self.feeder.take() {
            let _ = tokio::task::spawn_blocking(move || feeder.join()).await;
        }
    }

    async fn apply(&mut self, actions: Vec<TrackAction>) {
        for action in actions {
            match action {
                TrackAction::Download { url } => self.start_download(url),
                TrackAction::Decode { url } => {
                    let id = self.next_id();
                    self.start_decode(url, id, Arc::default());
                }
                TrackAction::Ack { url } => self.ack(url),
                // The planner evicts decoded tracks only, which have no load.
                TrackAction::Evict { url } => self.delete_decoded(&url).await,
                TrackAction::Forget { url } => self.forget(&url).await,
            }
        }
    }

    fn next_id(&mut self) -> u64 {
        self.next_load_id += 1;
        self.next_load_id
    }

    fn start_download(&mut self, url: String) {
        let id = self.next_id();
        let source = resolve_audio_url(&self.base, &url);
        let dest = self.cache_dir.join(track_file_name(&url));
        let client = self.client.clone();
        let done = self.done_tx.clone();
        let task = tokio::spawn({
            let url = url.clone();
            async move {
                let result = download_track(&client, source, &dest, &url).await;
                let step = Step::Downloaded(result);
                let _ = done.send(LoadDone { url, id, step });
            }
        });
        let cancel = Arc::default();
        self.loads.insert(url, Load { id, task, cancel });
    }

    /// Decodes the download of the URL on a blocking thread: the second step of a download,
    /// or a load of its own for a track whose decoded file was evicted.
    fn start_decode(&mut self, url: String, id: u64, cancel: Arc<AtomicBool>) {
        let source = self.cache_dir.join(track_file_name(&url));
        let dest = self.cache_dir.join(decoded_file_name(&url));
        let done = self.done_tx.clone();
        let task = tokio::task::spawn_blocking({
            let url = url.clone();
            let cancel = cancel.clone();
            move || {
                let step = Step::Decoded(decode_file(&source, &dest, &cancel, &url));
                let _ = done.send(LoadDone { url, id, step });
            }
        });
        self.loads.insert(url, Load { id, task, cancel });
    }

    /// A dropped ack makes the server start the play after its 3 s wait instead.
    fn ack(&mut self, url: String) {
        let ack = ClientMessage::AudioSourceLoaded {
            source: AudioSource { url },
        };
        self.send(Outgoing::Json(ack));
    }

    /// Never waits: a full or closed queue drops the message.
    fn send(&mut self, message: Outgoing) {
        if self.out.try_send(message).is_err() && !self.out_dropped {
            self.out_dropped = true;
            eprintln!(
                "streamer: the outgoing queue is full or closed, a message of the track loader \
                 was dropped"
            );
        }
    }

    /// Stops the URL's load before its files go. The file operations of an aborted download
    /// run on the blocking pool and can finish after the abort, so in rare cases a stale
    /// download stays until the next download of that URL or until leave.
    async fn forget(&mut self, url: &str) {
        if let Some(load) = self.loads.remove(url) {
            load.stop().await;
        }
        self.delete_decoded(url).await;
        let path = self.cache_dir.join(track_file_name(url));
        log_removal(tokio::fs::remove_file(&path).await, &path);
    }

    /// Deletes the decoded file of the URL. A reader that has it open (the feeder) keeps its
    /// data until it drops the file, so a playing track plays on.
    async fn delete_decoded(&mut self, url: &str) {
        self.decoded.remove(url);
        self.player.on_track_unready(url);
        let path = self.cache_dir.join(decoded_file_name(url));
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

/// Downloads one track to `dest`. The error is the text the queue shows; the cause goes to
/// the log.
async fn download_track(
    client: &reqwest::Client,
    source: Result<Url, HttpError>,
    dest: &Path,
    url: &str,
) -> Result<(), String> {
    let downloaded = match source {
        Ok(source) => download(client, &source, dest, MAX_TRACK_BYTES).await,
        Err(e) => Err(e),
    };
    match downloaded {
        Ok(_) => Ok(()),
        Err(e) => {
            eprintln!("streamer: cannot download the track {url}: {e}");
            Err(e.to_string())
        }
    }
}

/// Decodes the download at `source` into the decoded-track file `dest` and opens it.
/// Blocking. Once `cancel` is set, the decode stops before its next packet or write and leaves
/// no file. The error is the text the queue shows; the cause goes to the log.
fn decode_file(
    source: &Path,
    dest: &Path,
    cancel: &AtomicBool,
    url: &str,
) -> Result<PcmFile, String> {
    let decoded = std::panic::catch_unwind(AssertUnwindSafe(|| {
        decode_until_cancelled(source, dest, cancel)
    }));
    match decoded {
        Ok(Ok(_)) => PcmFile::open(dest).map_err(|e| {
            eprintln!("streamer: cannot open the decoded track {url}: {e}");
            "Cannot play this track".to_string()
        }),
        Ok(Err(e)) => {
            if !cancel.load(Ordering::Relaxed) {
                let cause = std::error::Error::source(&e).map(ToString::to_string);
                eprintln!(
                    "streamer: cannot decode the track {url}: {}",
                    cause.unwrap_or_default()
                );
            }
            Err(e.to_string())
        }
        // The decoder panicked: the same text as a `MediaError`.
        Err(_) => {
            eprintln!("streamer: decoding the track {url} panicked");
            Err("Cannot play this track".to_string())
        }
    }
}

fn decode_until_cancelled(
    source: &Path,
    dest: &Path,
    cancel: &AtomicBool,
) -> Result<u64, MediaError> {
    let writer = PcmFileWriter::create(dest).map_err(|e| MediaError::Unplayable(e.into()))?;
    let mut sink = CancellableWriter { writer, cancel };
    let frames = decode_to(source, &mut sink)?;
    sink.writer
        .finish()
        .map_err(|e| MediaError::Unplayable(e.into()))?;
    Ok(frames)
}

/// A decoded-track writer that stops the decode once `cancel` is set: the decoder checks it
/// before every packet, and every write fails. The dropped writer removes the part file.
struct CancellableWriter<'a> {
    writer: PcmFileWriter,
    cancel: &'a AtomicBool,
}

impl PcmSink for CancellableWriter<'_> {
    fn write(&mut self, stereo_48k: &[f32]) -> io::Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "the decode was cancelled",
            ));
        }
        self.writer.write(stereo_48k)
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// A xorshift64 generator for the shuffle draws.
struct XorShift(u64);

impl XorShift {
    fn from_clock() -> Self {
        Self::new(mono_ns() as u64)
    }

    /// A zero state would stay zero, so a zero seed is replaced.
    fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x9e37_79b9_7f4a_7c15
        } else {
            seed
        })
    }

    /// A number in [0, 1).
    fn next_f64(&mut self) -> f64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        (x >> 11) as f64 / (1u64 << 53) as f64
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use futures_util::{SinkExt, StreamExt};
    use serde_json::{Value, json};
    use tokio::sync::{mpsc, watch};
    use tokio_tungstenite::tungstenite::Message;

    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::super::feeder::FEED_AHEAD_MS;
    use super::super::{TrackState, TrackStates, decoded_file_name, track_file_name};
    use crate::audio::{OutputIo, PlayoutShared};
    use crate::clock::mono_ns;
    use crate::connection::JoinKind;
    use crate::http::test_server::{Reply, TestServer};
    use crate::media::PcmFile;
    use crate::mixer::{FADE_FRAMES, Slot};
    use crate::playout::PcmBlock;
    use crate::runtime::StreamerHandle;
    use crate::test_support::{
        FakeBackend, Room, WAIT, Ws, accept, connect_app, serve_room, server, settings, wait_until,
    };
    use crate::test_util::TempDir;

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

    fn load(url: &str) -> Value {
        json!({
            "type": "ROOM_EVENT",
            "event": { "type": "LOAD_AUDIO_SOURCE", "audioSourceToPlay": { "url": url } },
        })
    }

    fn play(url: &str) -> Value {
        json!({
            "type": "SCHEDULED_ACTION",
            "serverTimeToExecute": 1000,
            "scheduledAction": { "type": "PLAY", "audioSource": url, "trackTimeSeconds": 0 },
        })
    }

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/media")
                .join(name),
        )
        .unwrap()
    }

    /// A silent WAV file: 8 kHz, mono, unsigned 8-bit samples.
    fn silent_wav(seconds: u32) -> Vec<u8> {
        let rate = 8_000u32;
        let len = rate * seconds;
        let mut wav = Vec::with_capacity(44 + len as usize);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // channels
        wav.extend_from_slice(&rate.to_le_bytes());
        wav.extend_from_slice(&rate.to_le_bytes()); // bytes per second
        wav.extend_from_slice(&1u16.to_le_bytes()); // bytes per frame
        wav.extend_from_slice(&8u16.to_le_bytes()); // bits per sample
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&len.to_le_bytes());
        wav.resize(44 + len as usize, 128);
        wav
    }

    /// 3 s at the level 0.5: 48 kHz, mono, 16-bit samples.
    fn level_wav() -> Vec<u8> {
        let rate = 48_000u32;
        let len = 2 * rate * 3;
        let mut wav = Vec::with_capacity(44 + len as usize);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // channels
        wav.extend_from_slice(&rate.to_le_bytes());
        wav.extend_from_slice(&(2 * rate).to_le_bytes()); // bytes per second
        wav.extend_from_slice(&2u16.to_le_bytes()); // bytes per frame
        wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&len.to_le_bytes());
        for _ in 0..len / 2 {
            wav.extend_from_slice(&16_384i16.to_le_bytes());
        }
        wav
    }

    /// Serves the 2 s tone at /tone.mp3 and at any other /tone….mp3, a file that does not
    /// decode at /broken.mp3, an MP3 whose header length is a wrong estimate at /quiet.mp3,
    /// 10 minutes of silence at /long.wav, an HTML error page at /page.mp3, at /stalled.mp3
    /// a body that stops halfway, the 1 s click fixture at /click.wav and 3 s at the level 0.5
    /// at /level.wav; anything else is a 404.
    async fn track_server() -> TestServer {
        let tone = fixture("tone.mp3");
        let broken = fixture("broken.mp3");
        let quiet = fixture("quiet_start_noxing.mp3");
        let click = fixture("click_left.wav");
        let level = level_wav();
        TestServer::start(move |request| match request.target.as_str() {
            t if t.starts_with("/tone") && t.ends_with(".mp3") => Reply::bytes(tone.clone()),
            "/broken.mp3" => Reply::bytes(broken.clone()),
            "/quiet.mp3" => Reply::bytes(quiet.clone()),
            "/click.wav" => Reply::bytes(click.clone()),
            "/level.wav" => Reply::bytes(level.clone()),
            "/long.wav" => Reply::bytes(silent_wav(600)),
            "/page.mp3" => Reply::bytes(b"<!DOCTYPE html><html><body>Oops</body></html>".to_vec()),
            "/stalled.mp3" => Reply::bytes(vec![0; 100_000]).stalled_after(50_000),
            _ => Reply::text(404, "Not found"),
        })
        .await
    }

    /// The next `AUDIO_SOURCE_LOADED` the app sends; other messages are skipped.
    async fn next_ack(ws: &mut Ws) -> Value {
        let ack = async {
            loop {
                match ws.next().await {
                    Some(Ok(Message::Text(text))) => {
                        let message: Value = serde_json::from_str(text.as_str()).unwrap();
                        if message["type"] == "AUDIO_SOURCE_LOADED" {
                            return message;
                        }
                    }
                    Some(Ok(_)) => {}
                    other => panic!("the app closed the socket: {other:?}"),
                }
            }
        };
        tokio::time::timeout(WAIT, ack)
            .await
            .expect("the app acks the load request")
    }

    fn ack_message(url: &str) -> Value {
        json!({ "type": "AUDIO_SOURCE_LOADED", "source": { "url": url } })
    }

    fn decoded(cache: &TempDir, url: &str) -> std::path::PathBuf {
        cache.0.join(decoded_file_name(url))
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
        // The download and its decoded file stay while the track is in the queue.
        let mut files = [track_file_name(&url), decoded_file_name(&url)];
        files.sort();
        assert_eq!(cache.files(), files);
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn a_load_request_is_acked_after_decode() {
        // The server starts the room once every app acked. An ack before the decode ends
        // would start the room while the PC cannot play yet.
        let http = track_server().await;
        let url = http.url("/tone.mp3").to_string();
        let cache = TempDir::new("tracks-ack");
        let (handle, mut ws, _listener) = join_app(&cache).await;
        let tracks = handle.tracks();

        send(&mut ws, queue(&[&url])).await;
        send(&mut ws, load(&url)).await;
        assert_eq!(next_ack(&mut ws).await, ack_message(&url));
        assert!(decoded(&cache, &url).exists(), "the ack follows the decode");
        wait_until("the track is ready", || is_ready(&tracks, &url)).await;

        // A request for a decoded track is acked at once.
        send(&mut ws, load(&url)).await;
        assert_eq!(next_ack(&mut ws).await, ack_message(&url));
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn a_failed_decode_still_acks() {
        // Without the ack the room would wait the full 3 s for the PC on each play of the
        // track.
        let http = track_server().await;
        let url = http.url("/broken.mp3").to_string();
        let cache = TempDir::new("tracks-ack-error");
        let (handle, mut ws, _listener) = join_app(&cache).await;
        let tracks = handle.tracks();

        send(&mut ws, queue(&[&url])).await;
        send(&mut ws, load(&url)).await;
        assert_eq!(next_ack(&mut ws).await, ack_message(&url));
        wait_until("the track is an error", || {
            matches!(state(&tracks, &url), Some(TrackState::Error { .. }))
        })
        .await;
        assert_eq!(
            state(&tracks, &url),
            Some(TrackState::Error {
                message: "Cannot play this track".to_string()
            })
        );
        // No decoded file and no part file are left.
        assert_eq!(cache.files(), [track_file_name(&url)]);
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn ready_duration_comes_from_the_decoded_track() {
        // This MP3 has no length tag, and the length in its header is an estimate more than
        // twice too long. The queue and the end of the track follow the audio that plays.
        let http = track_server().await;
        let url = http.url("/quiet.mp3").to_string();
        let cache = TempDir::new("tracks-duration");
        let (handle, mut ws, _listener) = join_app(&cache).await;
        let tracks = handle.tracks();

        send(&mut ws, queue(&[&url])).await;
        send(&mut ws, play(&url)).await;
        wait_until("the track has loaded", || {
            !matches!(state(&tracks, &url), None | Some(TrackState::Loading))
        })
        .await;

        let Some(TrackState::Ready { duration_s }) = state(&tracks, &url) else {
            panic!("{:?}", state(&tracks, &url));
        };
        let pcm = PcmFile::open(&decoded(&cache, &url)).unwrap();
        assert_eq!(duration_s, pcm.duration_s());
        assert!((3.0..3.1).contains(&duration_s), "{duration_s}");
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn an_evicted_track_file_is_deleted() {
        // At most three decoded files stay on disk, at about 690 MB per hour of audio each.
        let http = track_server().await;
        let urls: Vec<String> = (1..=4)
            .map(|n| http.url(&format!("/tone{n}.mp3")).to_string())
            .collect();
        let cache = TempDir::new("tracks-evict");
        let (handle, mut ws, _listener) = join_app(&cache).await;
        let tracks = handle.tracks();

        let queued: Vec<&str> = urls.iter().map(String::as_str).collect();
        send(&mut ws, queue(&queued)).await;
        for url in &urls {
            send(&mut ws, play(url)).await;
            wait_until("the track is decoded", || decoded(&cache, url).exists()).await;
        }
        let first = &urls[0];
        wait_until("the least recently used decoded file is gone", || {
            !decoded(&cache, first).exists()
        })
        .await;
        assert!(
            cache.0.join(track_file_name(first)).exists(),
            "the download stays"
        );
        assert!(is_ready(&tracks, first));

        // Needed again, the track is decoded from its download before the ack.
        send(&mut ws, load(first)).await;
        assert_eq!(next_ack(&mut ws).await, ack_message(first));
        assert!(decoded(&cache, first).exists());
        let downloads = http
            .requests()
            .iter()
            .filter(|request| request.target == "/tone1.mp3")
            .count();
        assert_eq!(downloads, 1);
        wait_until("the next least recently used decoded file is gone", || {
            !decoded(&cache, &urls[1]).exists()
        })
        .await;
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn leave_deletes_the_decoded_files() {
        // A long decode that runs at leave stops at once: it neither holds up the leave nor
        // leaves a file behind.
        let http = track_server().await;
        let tone = http.url("/tone.mp3").to_string();
        let long = http.url("/long.wav").to_string();
        let cache = TempDir::new("tracks-leave-decoded");
        let (handle, mut ws, _listener) = join_app(&cache).await;
        let tracks = handle.tracks();

        send(&mut ws, queue(&[&tone, &long])).await;
        send(&mut ws, play(&tone)).await;
        wait_until("the tone is decoded", || decoded(&cache, &tone).exists()).await;
        send(&mut ws, play(&long)).await;
        let part = cache.0.join(format!("{}.part", decoded_file_name(&long)));
        wait_until("the long track is decoding", || part.exists()).await;

        tokio::time::timeout(WAIT, handle.shutdown())
            .await
            .expect("the leave does not wait for the decode");
        assert_eq!(cache.files(), Vec::<String>::new());
        assert!(tracks.borrow().is_empty());
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

    fn play_at(url: &str, at_ms: f64, position_s: f64) -> Value {
        json!({
            "type": "SCHEDULED_ACTION",
            "serverTimeToExecute": at_ms,
            "scheduledAction": { "type": "PLAY", "audioSource": url, "trackTimeSeconds": position_s },
        })
    }

    fn pause_at(url: &str, at_ms: f64) -> Value {
        json!({
            "type": "SCHEDULED_ACTION",
            "serverTimeToExecute": at_ms,
            "scheduledAction": { "type": "PAUSE", "audioSource": url, "trackTimeSeconds": 0 },
        })
    }

    /// The app joined to a fresh local room that answers its clock sync; returns once the
    /// server offset is known and the output is open.
    async fn join_synced(cache: &TempDir) -> (StreamerHandle, Room, Arc<FakeBackend>) {
        let (listener, settings) = server().await;
        let backend = Arc::new(FakeBackend::default());
        let handle = connect_app(settings, backend.clone(), None, cache).await;
        let room = serve_room(accept(&listener).await);
        let status = handle.status();
        wait_until("the server offset is known", || {
            status.borrow().offset_ms.is_some()
        })
        .await;
        wait_until("the output opens", || {
            backend.calls().contains(&"open_output")
        })
        .await;
        (handle, room, backend)
    }

    /// Puts the track in the queue and waits until it is decoded.
    async fn queue_ready(room: &Room, tracks: &watch::Receiver<TrackStates>, urls: &[&str]) {
        room.send(queue(urls));
        room.send(load(urls[0]));
        wait_until("the track is ready", || is_ready(tracks, urls[0])).await;
    }

    #[derive(Clone)]
    enum SlotEvent {
        Block(Box<PcmBlock>),
        /// A flush done at the server time `at_ms`, at once or at its stop time.
        Flush {
            stop_ms: Option<f64>,
            at_ms: f64,
        },
    }

    /// Plays the part of the output's track slot, with no delay: takes each block the feeder
    /// queues, does each flush at once or, with a stop time, once the server time reaches it
    /// (the mixer fades for 5 ms first), and records both in order.
    struct TrackSlot {
        shared: Arc<PlayoutShared>,
        events: Arc<Mutex<Vec<SlotEvent>>>,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl TrackSlot {
        fn run(output: OutputIo) -> Self {
            let OutputIo {
                mut track_rx,
                shared,
                ..
            } = output;
            let events = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let thread = std::thread::spawn({
                let (shared, events, stop) = (shared.clone(), events.clone(), stop.clone());
                move || {
                    let track = Slot::Track as usize;
                    while !stop.load(Ordering::Relaxed) {
                        let requested = shared.flush_requested[track].load(Ordering::Acquire);
                        let stop_at = &shared.stop_at_ms[track];
                        let stop_ms = Some(f64::from_bits(stop_at.load(Ordering::Acquire)))
                            .filter(|stop| !stop.is_nan());
                        let now = shared.mono_to_server_ms(mono_ns());
                        if requested != shared.flush_done[track].load(Ordering::Relaxed)
                            && stop_ms.is_none_or(|stop| now >= stop)
                        {
                            while track_rx.pop().is_ok() {}
                            stop_at.store(f64::NAN.to_bits(), Ordering::Release);
                            shared.flush_done[track].store(requested, Ordering::Release);
                            let flush = SlotEvent::Flush {
                                stop_ms,
                                at_ms: now,
                            };
                            events.lock().unwrap().push(flush);
                        }
                        while let Ok(block) = track_rx.pop() {
                            events
                                .lock()
                                .unwrap()
                                .push(SlotEvent::Block(Box::new(block)));
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            });
            Self {
                shared,
                events,
                stop,
                thread: Some(thread),
            }
        }

        fn server_now(&self) -> f64 {
            server_now(&self.shared)
        }

        fn events(&self) -> Vec<SlotEvent> {
            self.events.lock().unwrap().clone()
        }

        /// The targets of the blocks taken; a flush shows as `None`.
        fn targets(&self) -> Vec<Option<f64>> {
            self.events()
                .iter()
                .map(|event| match event {
                    SlotEvent::Block(block) => Some(block.target_start_server_ms),
                    SlotEvent::Flush { .. } => None,
                })
                .collect()
        }

        fn blocks(&self) -> Vec<PcmBlock> {
            self.events()
                .into_iter()
                .filter_map(|event| match event {
                    SlotEvent::Block(block) => Some(*block),
                    SlotEvent::Flush { .. } => None,
                })
                .collect()
        }

        fn has_block_at(&self, at_ms: f64) -> bool {
            self.targets().contains(&Some(at_ms))
        }
    }

    impl Drop for TrackSlot {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// The server time now in whole ms: the times the tests send must come back exactly
    /// from JSON, and a fraction does not always.
    fn server_now(shared: &PlayoutShared) -> f64 {
        shared.mono_to_server_ms(mono_ns()).round()
    }

    /// Block targets `start`, `start + 20`, … `start + 20·(count − 1)` ms.
    fn grid(start: f64, count: usize) -> Vec<Option<f64>> {
        (0..count).map(|k| Some(start + 20.0 * k as f64)).collect()
    }

    #[tokio::test]
    async fn a_scheduled_play_feeds_the_track_ring_at_its_target() {
        // The PC plays the queue track with the room: the frame at the PLAY's position is
        // heard at its server time, and the track runs on from there in exact 20 ms steps.
        let http = track_server().await;
        let url = http.url("/click.wav").to_string();
        let cache = TempDir::new("feed-play");
        let (handle, room, backend) = join_synced(&cache).await;
        let slot = TrackSlot::run(backend.take_output().expect("the output is open"));
        queue_ready(&room, &handle.tracks(), &[&url]).await;

        let t = slot.server_now() + 500.0;
        room.send(play_at(&url, t, 0.0));
        wait_until("the click's block is queued", || {
            slot.has_block_at(t + 500.0)
        })
        .await;

        assert_eq!(slot.targets()[..26], grid(t, 26));
        // The click starts at 0.500 s of the track: frame 0 of the block due at T + 500 ms.
        let blocks = slot.blocks();
        let loud = |block: &PcmBlock| block.samples.iter().any(|s| s.abs() > 0.5);
        let click = blocks.iter().find(|block| loud(block)).expect("the click");
        assert_eq!(click.target_start_server_ms, t + 500.0);
        assert!(click.samples[0].abs() > 0.5, "{}", click.samples[0]);
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn a_new_play_replaces_the_feed() {
        // A seek while the track plays: the queued blocks of the old position are flushed
        // (with the mixer's fade), and only the new position follows.
        let http = track_server().await;
        let url = http.url("/level.wav").to_string();
        let cache = TempDir::new("feed-replace");
        let (handle, room, backend) = join_synced(&cache).await;
        let slot = TrackSlot::run(backend.take_output().expect("the output is open"));
        queue_ready(&room, &handle.tracks(), &[&url]).await;

        let t1 = slot.server_now() + 200.0;
        room.send(play_at(&url, t1, 0.0));
        wait_until("the track is queued", || slot.has_block_at(t1 + 100.0)).await;
        let t2 = slot.server_now() + 400.0;
        room.send(play_at(&url, t2, 1.0));
        wait_until("the new position is queued", || {
            slot.has_block_at(t2 + 40.0)
        })
        .await;

        let targets = slot.targets();
        let flush = targets.iter().position(Option::is_none).expect("a flush");
        assert_eq!(targets[..flush], grid(t1, flush));
        assert_eq!(targets[flush + 1..flush + 4], grid(t2, 3));
        assert_eq!(
            targets.iter().filter(|t| t.is_none()).count(),
            1,
            "{targets:?}"
        );
        assert_eq!(
            slot.shared.flush_requested[Slot::Track as usize].load(Ordering::Acquire),
            1
        );
        assert_eq!(
            slot.shared.flush_requested[Slot::Stream as usize].load(Ordering::Acquire),
            0,
            "the local copy is not touched"
        );
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn a_pause_stops_the_feed_at_its_time() {
        // A PAUSE at T: the track fades out to silence at T, and nothing after T is queued.
        // A PAUSE due within the queued blocks: the output stops them itself at T.
        let http = track_server().await;
        let url = http.url("/level.wav").to_string();
        let cache = TempDir::new("feed-pause");
        let (handle, room, backend) = join_synced(&cache).await;
        let slot = TrackSlot::run(backend.take_output().expect("the output is open"));
        queue_ready(&room, &handle.tracks(), &[&url]).await;

        let t = slot.server_now() + 500.0;
        room.send(play_at(&url, t, 0.0));
        room.send(pause_at(&url, t + 310.0));
        // Long enough for the feeder to queue far past T + 310 ms if it did not stop.
        wait_until("the room is past the pause", || {
            slot.server_now() > t + 310.0 + 2.0 * FEED_AHEAD_MS
        })
        .await;
        assert_eq!(slot.targets(), grid(t, 16));
        // T + 310 ms is frame 480 of the last block.
        let last = slot.blocks()[15];
        assert!(
            last.samples[..2 * (480 - FADE_FRAMES)]
                .iter()
                .all(|&s| s == 0.5)
        );
        assert!(last.samples[2 * 479] > 0.0);
        assert!(last.samples[2 * 480..].iter().all(|&s| s == 0.0));

        let t = slot.server_now() + 150.0;
        room.send(play_at(&url, t, 0.0));
        wait_until("the track is queued", || slot.has_block_at(t + 100.0)).await;
        room.send(pause_at(&url, t + 100.0));
        wait_until("the queued blocks are stopped", || {
            slot.targets().last() == Some(&None)
        })
        .await;
        let Some(SlotEvent::Flush { stop_ms, at_ms }) = slot.events().last().cloned() else {
            unreachable!()
        };
        assert_eq!(
            stop_ms,
            Some(t + 100.0),
            "the output stops at the PAUSE's time"
        );
        // The fake output looks every 1 ms; neither at once nor long after T.
        assert!(
            (t + 100.0..t + 150.0).contains(&at_ms),
            "stopped {} ms after T",
            at_ms - (t + 100.0)
        );
        let targets = slot.targets();
        let flush = targets.len() - 1;
        assert_eq!(targets[16..flush], grid(t, flush - 16));
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn a_nudge_feeds_the_track_further_ahead() {
        // A positive nudge makes the output play every target earlier, so it needs the
        // blocks earlier too. Fed only 300 ms ahead of the server time, the output would
        // starve with a nudge above about 250 ms.
        let http = track_server().await;
        let url = http.url("/level.wav").to_string();
        let cache = TempDir::new("feed-nudge");
        let (handle, room, backend) = join_synced(&cache).await;
        let slot = TrackSlot::run(backend.take_output().expect("the output is open"));
        queue_ready(&room, &handle.tracks(), &[&url]).await;
        handle.set_nudge(500.0);
        wait_until("the nudge is set", || slot.shared.nudge_ms() == 500.0).await;

        // 360 ms more than the nudge and the start margin, so a slow run still starts at T.
        let t = slot.server_now() + 900.0;
        room.send(play_at(&url, t, 0.0));
        wait_until("the track is queued", || slot.has_block_at(t + 100.0)).await;
        // With the nudge this block is queued at about T − 700 ms; without it, at T − 200.
        assert!(slot.server_now() < t - 400.0, "queued 500 ms earlier");
        assert_eq!(slot.targets()[..6], grid(t, 6));
        handle.shutdown().await;
    }

    #[test]
    fn a_zero_seed_still_draws_numbers() {
        // A xorshift state of zero stays zero: shuffle would draw the first track forever.
        let mut rand = super::XorShift::new(0);
        let draws: Vec<f64> = (0..8).map(|_| rand.next_f64()).collect();
        assert!(draws.iter().all(|d| (0.0..1.0).contains(d)), "{draws:?}");
        assert!(draws.windows(2).all(|w| w[0] != w[1]), "{draws:?}");
    }

    #[tokio::test]
    async fn a_natural_end_sends_the_next_play() {
        // An admin's PC asks for the next track when the playing one ends, as the web does.
        let http = track_server().await;
        let first = http.url("/click.wav").to_string();
        let next = http.url("/level.wav").to_string();
        let cache = TempDir::new("feed-autoplay");
        let (handle, mut room, backend) = join_synced(&cache).await;
        let (shared, _) = backend.output_track().expect("the output is open");
        room.send(json!({
            "type": "ROOM_EVENT",
            "event": {
                "type": "CLIENT_CHANGE",
                "clients": [{ "clientId": "me", "username": "alice", "isAdmin": true }],
            },
        }));
        queue_ready(&room, &handle.tracks(), &[&first, &next]).await;

        let t = server_now(&shared) + 300.0;
        room.send(play_at(&first, t, 0.0));
        let sent = room.next_of("PLAY").await;
        let sent_at = shared.mono_to_server_ms(mono_ns());
        assert_eq!(
            sent,
            json!({ "type": "PLAY", "trackTimeSeconds": 0, "audioSource": next })
        );
        // The click is 1 s long.
        assert!(sent_at >= t + 999.0, "sent {} ms after T", sent_at - t);
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn leave_stops_the_feeder_and_closes_the_output() {
        // Leave while the track plays: the track fades out, its feeder ends before the output
        // closes, and the decoded file goes.
        let http = track_server().await;
        let url = http.url("/level.wav").to_string();
        let cache = TempDir::new("feed-leave");
        let (handle, room, backend) = join_synced(&cache).await;
        queue_ready(&room, &handle.tracks(), &[&url]).await;
        let (shared, _) = backend.output_track().expect("the output is open");

        room.send(play_at(&url, server_now(&shared) + 100.0, 0.0));
        wait_until("the track is queued", || {
            backend
                .output_track()
                .is_some_and(|(_, queued)| queued >= 3)
        })
        .await;
        tokio::time::timeout(WAIT, handle.shutdown())
            .await
            .expect("the leave does not hang");

        assert_eq!(backend.calls().last(), Some(&"close_output"));
        assert_eq!(backend.track_producer_gone_at_close(), Some(true));
        assert_eq!(
            shared.flush_requested[Slot::Track as usize].load(Ordering::Acquire),
            1,
            "the track fades out"
        );
        assert_eq!(cache.files(), Vec::<String>::new());
    }
}
