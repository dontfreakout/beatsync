//! PipeWire backend (Linux, spec §9.3): needs PipeWire 1.0 and WirePlumber 0.4.17 or later.

use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ::pipewire as pw;

use super::{AppInfo, AudioBackend, AudioError, AudioIo, OutputIo};

mod capture;
mod media;
mod playback;
mod routing;
mod thread;

use thread::{Command, Reply, Request};

/// `application.name` of the streamer's PipeWire connection and streams; streams with it are
/// never listed or moved.
pub const OWN_APP_NAME: &str = "Beatsync Streamer";

/// Wait for the core info and the WirePlumber client at startup.
const INIT_TIMEOUT: Duration = Duration::from_secs(2);
/// Wait for the sink to show up in the registry.
const SINK_TIMEOUT: Duration = Duration::from_secs(2);
/// Wait for the server to confirm the restore and the sink removal.
const STOP_TIMEOUT: Duration = Duration::from_secs(2);
/// Routing runs this long after a stream appears, changes or vanishes, or the selection
/// changes. A stream of a selected app that is not on the sink yet is routed at once.
const ROUTE_DEBOUNCE: Duration = Duration::from_millis(200);
/// A new stream's `target.object` entry that changes this soon after the stream appeared was
/// written by WirePlumber's stream-restore (it reacts to new streams a little after the
/// streamer), not by the user: the stream is moved to the sink again.
const RESTORE_RACE: Duration = Duration::from_secs(1);
/// Longer than every wait of the thread, so a reply only times out when the thread hangs.
const REPLY_TIMEOUT: Duration = Duration::from_secs(4);

/// The PipeWire backend. All PipeWire objects live on its own thread; the methods block until
/// that thread answers. Dropping it stops routing (if started), removes the sink, closes the
/// output and ends the thread.
pub struct PipeWireBackend {
    commands: pw::channel::Sender<Request>,
    thread: Option<JoinHandle<()>>,
}

impl PipeWireBackend {
    /// Connects to PipeWire and checks the PipeWire and WirePlumber versions (up to 2 s).
    pub fn new() -> Result<Self, AudioError> {
        let (commands, command_rx) = pw::channel::channel();
        let (init_tx, init_rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("pipewire".to_string())
            .spawn(move || thread::run(command_rx, init_tx))
            .map_err(|e| AudioError::Backend(format!("Cannot start the PipeWire thread: {e}")))?;
        let backend = Self {
            commands,
            thread: Some(thread),
        };
        let result = init_rx
            .recv_timeout(REPLY_TIMEOUT)
            .unwrap_or_else(|_| Err(no_answer()));
        // On an error `backend` drops here and joins the thread.
        result.map(|()| backend)
    }

    fn call<T>(&self, command: impl FnOnce(Reply<T>) -> Command) -> Result<T, AudioError> {
        if self.thread.as_ref().is_none_or(|t| t.is_finished()) {
            return Err(AudioError::Backend(
                "The PipeWire thread has stopped".to_string(),
            ));
        }
        let (reply, answer) = mpsc::channel();
        // Expires before `recv_timeout` gives up, so the thread never runs it unheard.
        let request = Request {
            command: command(reply),
            expires: Some(Instant::now() + REPLY_TIMEOUT),
        };
        self.commands.send(request).map_err(|_| no_answer())?;
        answer
            .recv_timeout(REPLY_TIMEOUT)
            .unwrap_or_else(|_| Err(no_answer()))
    }
}

fn no_answer() -> AudioError {
    AudioError::Backend("PipeWire did not answer".to_string())
}

impl AudioBackend for PipeWireBackend {
    fn list_apps(&self) -> Result<Vec<AppInfo>, AudioError> {
        self.call(Command::ListApps)
    }

    fn start(&self, apps: Vec<String>, io: AudioIo) -> Result<(), AudioError> {
        self.call(|reply| Command::Start { apps, io, reply })
    }

    fn update_apps(&self, apps: Vec<String>) -> Result<(), AudioError> {
        self.call(|reply| Command::UpdateApps { apps, reply })
    }

    fn stop(&self) -> Result<(), AudioError> {
        self.call(Command::Stop)
    }

    fn open_output(&self, io: OutputIo) -> Result<(), AudioError> {
        self.call(|reply| Command::OpenOutput {
            io,
            target: None,
            reply,
        })
    }

    fn close_output(&self) -> Result<(), AudioError> {
        self.call(Command::CloseOutput)
    }
}

impl Drop for PipeWireBackend {
    fn drop(&mut self) {
        let _ = self.commands.send(Request {
            command: Command::Quit,
            expires: None,
        });
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    //! Integration tests against the live PipeWire session of this machine. They are ignored;
    //! run them with:
    //!
    //! ```text
    //! cd apps/streamer && cargo test -p streamer-core -- --ignored pipewire
    //! ```
    //!
    //! `bun run core:test:ubuntu24` (in `apps/streamer`) runs them in a container with
    //! PipeWire 1.0.5 and WirePlumber 0.4.17.
    //!
    //! They need PipeWire 1.0+, WirePlumber 0.4.17+ and `pw-cat`, `pw-dump`, `pw-link`,
    //! `pw-metadata`, `pw-cli`. They touch only what they create: their own sinks, the
    //! streamer's capture and output streams (the output plays silence to a test sink of its
    //! own: its rings stay empty, its gain is 0 and the metronome is off), and `pw-cat`
    //! streams that play silence (zeros). Other streams and the default devices stay
    //! as they are. The tests run one at a time (`LOCK`), and every child process is killed on
    //! exit, also when a test fails.

    use std::collections::BTreeMap;
    use std::io::Write;
    use std::process::{Child, Command as Process, Stdio};
    use std::sync::{Arc, Mutex, MutexGuard};

    use serde_json::Value;
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    use super::capture::CAPTURE_NAME;
    use super::playback::{NO_LOCAL_OUTPUT, PLAYBACK_NAME};
    use super::*;
    use crate::audio::{AudioEvent, PlayoutShared};
    use crate::dsp::Metronome;

    static LOCK: Mutex<()> = Mutex::new(());
    /// Set for the child process of `sink_disappears_when_the_owning_connection_closes`.
    const CHILD_ENV: &str = "BEATSYNC_SINK_CHILD";

    pub(super) fn lock() -> MutexGuard<'static, ()> {
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn test_io() -> (AudioIo, UnboundedReceiver<AudioEvent>) {
        let (capture_tx, _) = rtrb::RingBuffer::new(4);
        let (events, events_rx) = unbounded_channel();
        let io = AudioIo {
            capture_tx,
            shared: Arc::new(PlayoutShared::default()),
            events,
        };
        (io, events_rx)
    }

    /// The ends of a silent output: empty rings, gain 0, no metronome.
    fn test_output() -> (OutputIo, UnboundedReceiver<AudioEvent>) {
        let (_, stream_rx) = rtrb::RingBuffer::new(4);
        let (_, track_rx) = rtrb::RingBuffer::new(4);
        let (events, events_rx) = unbounded_channel();
        let io = OutputIo {
            stream_rx,
            track_rx,
            metronome: Metronome::kick().expect("kick"),
            lowpass: true,
            shared: Arc::new(PlayoutShared::default()),
            events,
        };
        (io, events_rx)
    }

    /// Creates a null sink on the backend's connection, so it goes away with the backend,
    /// and returns its name once it is in the registry.
    fn test_sink(backend: &PipeWireBackend) -> String {
        let name = format!("beatsync-test-{}-out", std::process::id());
        let created = backend.call(|reply| Command::CreateSink {
            name: name.clone(),
            reply,
        });
        created.expect("test sink");
        assert!(
            wait_until(Duration::from_secs(3), || node_id(&name).is_some()),
            "the test sink appears"
        );
        name
    }

    /// Opens a silent output pinned to `sink`, never to a real device, and waits until it
    /// plays there.
    fn open_test_output(backend: &PipeWireBackend, sink: &str) -> UnboundedReceiver<AudioEvent> {
        let (io, events) = test_output();
        let opened = backend.call(|reply| Command::OpenOutput {
            io,
            target: Some(sink.to_string()),
            reply,
        });
        opened.expect("open_output");
        assert!(
            wait_until(Duration::from_secs(3), || output_only_to(sink)),
            "the output plays to the test sink only: {:?}",
            own_links(PLAYBACK_NAME)
        );
        events
    }

    /// Kills and reaps the child when dropped.
    struct ChildGuard(Child);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// Plays silence as app `name` (also its `node.name`). WirePlumber stores nothing for it.
    /// `pw-cat` reads a WAV stream from stdin: [`silent_wav_header`], then zeros until the
    /// guard kills it. pw-cat 1.0 has no raw mode (`--raw`) and cannot read a headerless file.
    fn play_silence(name: &str) -> ChildGuard {
        let props = format!(
            "{{ application.name = \"{name}\" node.name = \"{name}\" \
             state.restore-props = false state.restore-target = false }}"
        );
        let mut child = Process::new("pw-cat")
            .args(["-p", "--properties", &props, "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("pw-cat");
        let mut stdin = child.stdin.take().expect("pw-cat stdin");
        std::thread::spawn(move || {
            // A write fails (EPIPE) once pw-cat is gone, which ends the thread.
            let zeros = [0u8; 4096];
            let _ = stdin.write_all(&silent_wav_header());
            while stdin.write_all(&zeros).is_ok() {}
        });
        ChildGuard(child)
    }

    /// Header of a 16-bit stereo 48 kHz PCM WAV stream with the largest data length (6 h).
    fn silent_wav_header() -> Vec<u8> {
        let data_len = u32::MAX - 36;
        [
            &b"RIFF"[..],
            &(data_len + 36).to_le_bytes(),
            b"WAVEfmt ",
            &16u32.to_le_bytes(), // size of the fmt chunk
            &1u16.to_le_bytes(),  // PCM
            &2u16.to_le_bytes(),  // channels
            &48_000u32.to_le_bytes(),
            &192_000u32.to_le_bytes(), // bytes per second
            &4u16.to_le_bytes(),       // bytes per frame
            &16u16.to_le_bytes(),      // bits per sample
            b"data",
            &data_len.to_le_bytes(),
        ]
        .concat()
    }

    pub(super) fn output(program: &str, args: &[&str]) -> String {
        let out = Process::new(program).args(args).output().expect(program);
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// The objects in `pw-dump`. pw-dump 1.0 also prints `[{ "id": N, "info": null }]` for
    /// each object removed while it runs; these entries have no `type`.
    fn dump_objects() -> Vec<Value> {
        let out = output("pw-dump", &[]);
        serde_json::Deserializer::from_str(&out)
            .into_iter::<Value>()
            .flat_map(|doc| match doc.expect("pw-dump JSON") {
                Value::Array(objects) => objects,
                _ => Vec::new(),
            })
            .collect()
    }

    /// `(id, node.name)` of every node in `pw-dump`.
    pub(super) fn nodes() -> Vec<(u64, String)> {
        dump_objects()
            .iter()
            .filter(|o| o["type"] == "PipeWire:Interface:Node")
            .filter_map(|o| {
                let name = o["info"]["props"]["node.name"].as_str()?;
                Some((o["id"].as_u64()?, name.to_string()))
            })
            .collect()
    }

    /// Names of the sinks the backend in process `pid` created.
    fn sinks_of(pid: u32) -> Vec<String> {
        let prefix = format!("beatsync-{pid}-");
        nodes()
            .into_iter()
            .map(|(_, name)| name)
            .filter(|name| name.starts_with(&prefix))
            .collect()
    }

    fn node_id(name: &str) -> Option<u32> {
        let id = nodes().into_iter().find(|(_, n)| n == name)?.0;
        u32::try_from(id).ok()
    }

    fn node_serial(name: &str) -> Option<u64> {
        let objects = dump_objects();
        let node = objects.iter().find(|o| {
            o["type"] == "PipeWire:Interface:Node" && o["info"]["props"]["node.name"] == name
        })?;
        node["info"]["props"]["object.serial"].as_u64()
    }

    /// The `pw-metadata -n default` line of `key` on subject `id`.
    fn metadata_line(id: u32, key: &str) -> Option<String> {
        let needle = format!("id:{id} key:'{key}'");
        output("pw-metadata", &["-n", "default"])
            .lines()
            .find(|line| line.contains(&needle))
            .map(str::to_string)
    }

    fn default_sink_name() -> String {
        let line = metadata_line(0, "default.audio.sink").expect("a default sink");
        let json = line
            .split("value:'")
            .nth(1)
            .and_then(|rest| rest.split("' type:").next())
            .expect("a value");
        let value: Value = serde_json::from_str(json).expect("default sink JSON");
        value["name"].as_str().expect("a sink name").to_string()
    }

    /// Ports that the output ports of node `name` link to, per `pw-link -l`.
    pub(super) fn links_from(name: &str) -> Vec<String> {
        let prefix = format!("{name}:");
        let mut links = Vec::new();
        let mut in_node = false;
        for line in output("pw-link", &["-l"]).lines() {
            if !line.starts_with(char::is_whitespace) {
                in_node = line.starts_with(&prefix);
            } else if in_node && let Some(port) = line.trim().strip_prefix("|-> ") {
                links.push(port.to_string());
            }
        }
        links
    }

    fn linked_only_to(stream: &str, node: &str) -> bool {
        let links = links_from(stream);
        !links.is_empty() && links.iter().all(|p| p.starts_with(&format!("{node}:")))
    }

    pub(super) fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let end = Instant::now() + timeout;
        loop {
            if condition() {
                return true;
            }
            if Instant::now() >= end {
                return false;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn wait_listed(backend: &PipeWireBackend, app: &str) {
        let listed = wait_until(Duration::from_secs(3), || {
            let apps = backend.list_apps().expect("list_apps");
            apps.iter().any(|a| a.name == app)
        });
        assert!(listed, "{app} is listed: {:?}", backend.list_apps());
    }

    #[test]
    #[ignore = "needs a live PipeWire session"]
    fn sink_appears_and_disappears() {
        let _lock = lock();
        let defaults = || {
            let sink = metadata_line(0, "default.audio.sink");
            let source = metadata_line(0, "default.audio.source");
            (sink, source)
        };
        let before = defaults();
        let pid = std::process::id();
        let backend = PipeWireBackend::new().expect("backend");
        let (io, _events) = test_io();
        backend.start(Vec::new(), io).expect("start");
        assert_eq!(sinks_of(pid).len(), 1, "one sink while started");
        // WirePlumber re-evaluates the defaults when a sink appears.
        std::thread::sleep(Duration::from_secs(1));
        assert_eq!(defaults(), before, "the sink never becomes a default");
        drop(backend);
        assert!(
            wait_until(Duration::from_secs(2), || sinks_of(pid).is_empty()),
            "the sink is removed with the backend"
        );
    }

    /// Runs only as the child process of `sink_disappears_when_the_owning_connection_closes`.
    #[test]
    #[ignore = "helper process"]
    fn sink_owner_child() {
        if std::env::var_os(CHILD_ENV).is_none() {
            return;
        }
        let backend = PipeWireBackend::new().expect("backend");
        let (io, _events) = test_io();
        backend.start(Vec::new(), io).expect("start");
        // The parent kills this process long before.
        std::thread::sleep(Duration::from_secs(30));
    }

    #[test]
    #[ignore = "needs a live PipeWire session"]
    fn sink_disappears_when_the_owning_connection_closes() {
        let _lock = lock();
        let exe = std::env::current_exe().expect("test binary");
        let child = Process::new(exe)
            .args([
                "--ignored",
                "--exact",
                "audio::pipewire::tests::sink_owner_child",
            ])
            .env(CHILD_ENV, "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("child");
        let mut child = ChildGuard(child);
        let pid = child.0.id();
        assert!(
            wait_until(Duration::from_secs(10), || sinks_of(pid).len() == 1),
            "the child creates its sink"
        );
        child.0.kill().expect("SIGKILL");
        child.0.wait().expect("reap");
        assert!(
            wait_until(Duration::from_secs(3), || sinks_of(pid).is_empty()),
            "the sink is removed when its connection closes"
        );
    }

    #[test]
    #[ignore = "needs a live PipeWire session"]
    fn moves_and_restores_a_pw_cat_stream() {
        let _lock = lock();
        let app = format!("beatsync-test-{}", std::process::id());
        let _tone = play_silence(&app);
        let backend = PipeWireBackend::new().expect("backend");
        wait_listed(&backend, &app);
        let id = node_id(&app).expect("stream node");
        let default = default_sink_name();
        assert!(
            wait_until(Duration::from_secs(3), || linked_only_to(&app, &default)),
            "starts on the default sink: {:?}",
            links_from(&app)
        );

        let (io, _events) = test_io();
        backend.start(vec![app.clone()], io).expect("start");
        let sink = sinks_of(std::process::id()).pop().expect("sink");
        assert!(
            wait_until(Duration::from_secs(3), || linked_only_to(&app, &sink)),
            "moved to the sink: {:?}",
            links_from(&app)
        );

        backend.stop().expect("stop");
        assert!(
            wait_until(Duration::from_secs(3), || linked_only_to(&app, &default)),
            "back on the default sink: {:?}",
            links_from(&app)
        );
        assert_eq!(metadata_line(id, "target.object"), None);
    }

    #[test]
    #[ignore = "needs a live PipeWire session"]
    fn restores_the_previous_target_of_a_pw_cat_stream() {
        let _lock = lock();
        let app = format!("beatsync-test-{}", std::process::id());
        let _tone = play_silence(&app);
        let backend = PipeWireBackend::new().expect("backend");
        wait_listed(&backend, &app);
        let id = node_id(&app).expect("stream node");
        // "-1" means "no defined target": the stream stays on the default sink.
        let subject = id.to_string();
        let args = [
            "-n",
            "default",
            "--",
            &subject,
            "target.object",
            "-1",
            "Spa:Id",
        ];
        output("pw-metadata", &args);
        let previous = |line: Option<String>| line.is_some_and(|l| l.contains("value:'-1'"));
        assert!(
            wait_until(Duration::from_secs(3), || previous(metadata_line(
                id,
                "target.object"
            ))),
            "test target set"
        );

        let (io, _events) = test_io();
        backend.start(vec![app.clone()], io).expect("start");
        let sink = sinks_of(std::process::id()).pop().expect("sink");
        assert!(
            wait_until(Duration::from_secs(3), || linked_only_to(&app, &sink)),
            "moved to the sink: {:?}",
            links_from(&app)
        );

        backend.stop().expect("stop");
        assert!(
            previous(metadata_line(id, "target.object")),
            "previous target restored: {:?}",
            metadata_line(id, "target.object")
        );
    }

    #[test]
    #[ignore = "needs a live PipeWire session"]
    fn follows_new_streams_of_a_selected_app() {
        let _lock = lock();
        let app = format!("beatsync-test-{}", std::process::id());
        let backend = PipeWireBackend::new().expect("backend");
        let (io, mut events) = test_io();
        backend.start(vec![app.clone()], io).expect("start");
        let sink = sinks_of(std::process::id()).pop().expect("sink");

        // The app starts playing after the start.
        let _tone = play_silence(&app);
        assert!(
            wait_until(Duration::from_secs(3), || linked_only_to(&app, &sink)),
            "a new stream of the app moves to the sink: {:?}",
            links_from(&app)
        );
        backend.update_apps(Vec::new()).expect("update_apps");
        let default = default_sink_name();
        assert!(
            wait_until(Duration::from_secs(3), || linked_only_to(&app, &default)),
            "unselecting moves it back: {:?}",
            links_from(&app)
        );
        let id = node_id(&app).expect("stream node");
        assert_eq!(metadata_line(id, "target.object"), None);
        backend.stop().expect("stop");
        assert!(events.try_recv().is_err(), "no failure reported");
    }

    /// Polls `pw-link -l` (about 5 ms per call) from now until `stream` links only to `sink`,
    /// and returns how long it was linked elsewhere before that, with the samples.
    fn time_linked_elsewhere(stream: &str, sink: &str) -> (Duration, Vec<String>) {
        let begin = Instant::now();
        let mut first_link: Option<Instant> = None;
        let mut samples = Vec::new();
        while begin.elapsed() < Duration::from_secs(3) {
            let links = links_from(stream);
            let at = Instant::now();
            samples.push(format!(
                "{:>6.1} ms {links:?}",
                (at - begin).as_secs_f64() * 1e3
            ));
            if links.is_empty() {
                continue;
            }
            let first = *first_link.get_or_insert(at);
            if links.iter().all(|p| p.starts_with(&format!("{sink}:"))) {
                return (at - first, samples);
            }
        }
        panic!("{stream} never linked only to {sink}: {samples:#?}");
    }

    #[test]
    #[ignore = "needs a live PipeWire session"]
    fn new_stream_of_a_selected_app_goes_to_the_sink_at_once() {
        let _lock = lock();
        let app = format!("beatsync-test-{}", std::process::id());
        let backend = PipeWireBackend::new().expect("backend");
        let (io, mut events) = test_io();
        backend.start(vec![app.clone()], io).expect("start");
        let sink = sinks_of(std::process::id()).pop().expect("sink");

        // E.g. a browser tab that resumes: until the move, the app is heard undelayed.
        let _tone = play_silence(&app);
        let (elsewhere, samples) = time_linked_elsewhere(&app, &sink);
        eprintln!("linked elsewhere for {elsewhere:?}: {samples:#?}");
        assert!(
            elsewhere <= Duration::from_millis(50),
            "linked elsewhere for {elsewhere:?}: {samples:#?}"
        );
        backend.stop().expect("stop");
        assert!(events.try_recv().is_err(), "no failure reported");
    }

    #[test]
    #[ignore = "needs a live PipeWire session"]
    fn target_restored_right_after_a_new_stream_appears_is_overridden() {
        let _lock = lock();
        let app = format!("beatsync-test-{}", std::process::id());
        let backend = PipeWireBackend::new().expect("backend");
        let (io, mut events) = test_io();
        backend.start(vec![app.clone()], io).expect("start");
        let sink = sinks_of(std::process::id()).pop().expect("sink");
        let sink_serial = node_serial(&sink).expect("sink serial").to_string();
        let default = default_sink_name();
        let serial = node_serial(&default)
            .expect("default sink serial")
            .to_string();

        let _tone = play_silence(&app);
        let mut id = None;
        assert!(
            wait_until(Duration::from_secs(3), || {
                id = node_id(&app);
                id.is_some()
            }),
            "the stream appears"
        );
        let id = id.expect("stream node");
        // What WirePlumber's stream-restore does for an app with a remembered target, a
        // moment after the stream appeared: after the streamer's own move.
        let subject = id.to_string();
        let args = [
            "-n",
            "default",
            "--",
            &subject,
            "target.object",
            &serial,
            "Spa:Id",
        ];
        output("pw-metadata", &args);
        let on_sink = || {
            let line = metadata_line(id, "target.object");
            line.is_some_and(|l| l.contains(&format!("value:'{sink_serial}'")))
                && linked_only_to(&app, &sink)
        };
        assert!(
            wait_until(Duration::from_secs(3), on_sink),
            "moved to the sink again: {:?} {:?}",
            metadata_line(id, "target.object"),
            links_from(&app)
        );
        // Still there after a routing pass: the restore was not taken for a user's move.
        backend.update_apps(vec![app.clone()]).expect("update_apps");
        std::thread::sleep(Duration::from_millis(600));
        assert!(on_sink(), "{:?}", metadata_line(id, "target.object"));

        backend.stop().expect("stop");
        let restored = metadata_line(id, "target.object");
        assert!(
            restored
                .as_ref()
                .is_some_and(|l| l.contains(&format!("value:'{serial}'"))),
            "the restored target is the one to go back to: {restored:?}"
        );
        assert!(events.try_recv().is_err(), "no failure reported");
    }

    #[test]
    #[ignore = "needs a live PipeWire session"]
    fn user_move_away_is_kept_on_stop() {
        let _lock = lock();
        let app = format!("beatsync-test-{}", std::process::id());
        let _tone = play_silence(&app);
        let backend = PipeWireBackend::new().expect("backend");
        wait_listed(&backend, &app);
        let id = node_id(&app).expect("stream node");
        let (io, _events) = test_io();
        backend.start(vec![app.clone()], io).expect("start");
        let sink = sinks_of(std::process::id()).pop().expect("sink");
        assert!(
            wait_until(Duration::from_secs(3), || linked_only_to(&app, &sink)),
            "moved to the sink: {:?}",
            links_from(&app)
        );

        // The user moves the stream to the real default sink, as pavucontrol or pw-metadata do.
        let default = default_sink_name();
        let serial = node_serial(&default)
            .expect("default sink serial")
            .to_string();
        let subject = id.to_string();
        let args = [
            "-n",
            "default",
            "--",
            &subject,
            "target.object",
            &serial,
            "Spa:Id",
        ];
        output("pw-metadata", &args);
        let users_target = || {
            let line = metadata_line(id, "target.object");
            line.is_some_and(|l| l.contains(&format!("value:'{serial}'")))
        };
        assert!(
            wait_until(Duration::from_secs(3), || {
                users_target() && linked_only_to(&app, &default)
            }),
            "the user's move: {:?}",
            links_from(&app)
        );

        // A routing pass while the app stays selected does not move it back.
        backend.update_apps(vec![app.clone()]).expect("update_apps");
        std::thread::sleep(Duration::from_millis(600));
        assert!(
            users_target() && linked_only_to(&app, &default),
            "kept while streaming: {:?} {:?}",
            metadata_line(id, "target.object"),
            links_from(&app)
        );

        backend.stop().expect("stop");
        assert!(
            users_target(),
            "stop keeps the user's target: {:?}",
            metadata_line(id, "target.object")
        );
        assert!(linked_only_to(&app, &default), "{:?}", links_from(&app));
    }

    /// Sends `Stop` and `Start` back to back, so the start waits in the queue behind the stop.
    fn stop_then_start(
        backend: &PipeWireBackend,
        io: AudioIo,
    ) -> (Result<(), AudioError>, Result<(), AudioError>) {
        let expires = Some(Instant::now() + REPLY_TIMEOUT);
        let (stop_reply, stopped) = mpsc::channel();
        let (start_reply, started) = mpsc::channel();
        let stop = Command::Stop(stop_reply);
        let start = Command::Start {
            apps: Vec::new(),
            io,
            reply: start_reply,
        };
        for command in [stop, start] {
            let sent = backend.commands.send(Request { command, expires });
            assert!(sent.is_ok(), "command sent");
        }
        let answer = |rx: mpsc::Receiver<Result<(), AudioError>>| {
            rx.recv_timeout(REPLY_TIMEOUT)
                .unwrap_or_else(|_| Err(no_answer()))
        };
        (answer(stopped), answer(started))
    }

    #[test]
    #[ignore = "needs a live PipeWire session"]
    fn start_right_after_stop_succeeds() {
        let _lock = lock();
        let pid = std::process::id();
        let backend = PipeWireBackend::new().expect("backend");
        let (io, _events) = test_io();
        backend.start(Vec::new(), io).expect("start");
        assert_eq!(sinks_of(pid), vec![format!("beatsync-{pid}-1")]);

        let (io, _events) = test_io();
        let begin = Instant::now();
        let (stopped, started) = stop_then_start(&backend, io);
        let took = begin.elapsed();
        assert_eq!(stopped, Ok(()));
        assert_eq!(started, Ok(()));
        assert!(
            took < Duration::from_secs(1),
            "the start does not wait for an unrelated wake-up: {took:?}"
        );
        assert_eq!(sinks_of(pid), vec![format!("beatsync-{pid}-2")]);
        drop(backend);
        assert!(wait_until(Duration::from_secs(2), || sinks_of(pid).is_empty()));
    }

    #[test]
    #[ignore = "needs a live PipeWire session"]
    fn reports_a_rejected_metadata_write() {
        let _lock = lock();
        let backend = PipeWireBackend::new().expect("backend");
        let (io, mut events) = test_io();
        backend.start(Vec::new(), io).expect("start");
        // No object has this id, so the server refuses the write.
        let write = backend.call(|reply| Command::WriteTarget {
            subject: 999_999,
            reply,
        });
        write.expect("write sent");
        let failed = wait_until(
            Duration::from_secs(3),
            || matches!(events.try_recv(), Ok(AudioEvent::Failed(m)) if m.contains("refused")),
        );
        assert!(failed, "the runtime hears about the refused write");
        backend.stop().expect("stop");
    }

    /// Ids of this process's nodes named `name`.
    fn own_nodes(name: &str) -> Vec<u32> {
        own_nodes_in(&dump_objects(), name)
    }

    /// `node.name`s of the nodes that this process's nodes named `name` link to, one per
    /// link. Same-named nodes of other processes (a running streamer) do not count.
    pub(super) fn own_links(name: &str) -> Vec<String> {
        let objects = dump_objects();
        let own = own_nodes_in(&objects, name);
        let names: BTreeMap<u64, &str> = objects
            .iter()
            .filter(|o| o["type"] == "PipeWire:Interface:Node")
            .filter_map(|o| Some((o["id"].as_u64()?, o["info"]["props"]["node.name"].as_str()?)))
            .collect();
        objects
            .iter()
            .filter(|o| o["type"] == "PipeWire:Interface:Link")
            .filter(|o| {
                let from = o["info"]["output-node-id"].as_u64();
                from.is_some_and(|id| own.iter().any(|own| u64::from(*own) == id))
            })
            .filter_map(|o| names.get(&o["info"]["input-node-id"].as_u64()?))
            .map(|name| name.to_string())
            .collect()
    }

    /// Whether this process's output is linked, and only to `sink`.
    fn output_only_to(sink: &str) -> bool {
        let links = own_links(PLAYBACK_NAME);
        !links.is_empty() && links.iter().all(|n| n == sink)
    }

    fn own_nodes_in(objects: &[Value], name: &str) -> Vec<u32> {
        let pid = u64::from(std::process::id());
        // Stream nodes carry the process id only on their client.
        let clients: Vec<u64> = objects
            .iter()
            .filter(|o| o["type"] == "PipeWire:Interface:Client")
            .filter(|o| o["info"]["props"]["application.process.id"].as_u64() == Some(pid))
            .filter_map(|o| o["id"].as_u64())
            .collect();
        objects
            .iter()
            .filter(|o| o["type"] == "PipeWire:Interface:Node")
            .filter(|o| o["info"]["props"]["node.name"] == name)
            .filter(|o| {
                let client = o["info"]["props"]["client.id"].as_u64();
                client.is_some_and(|c| clients.contains(&c))
            })
            .filter_map(|o| u32::try_from(o["id"].as_u64()?).ok())
            .collect()
    }

    #[test]
    #[ignore = "needs a live PipeWire session"]
    fn start_captures_the_sink_while_the_output_plays() {
        let _lock = lock();
        let backend = PipeWireBackend::new().expect("backend");
        let out_sink = test_sink(&backend);
        let mut output_events = open_test_output(&backend, &out_sink);
        let (io, mut events) = test_io();
        backend.start(Vec::new(), io).expect("start");
        let sink = sinks_of(std::process::id()).pop().expect("sink");
        assert!(
            wait_until(Duration::from_secs(3), || linked_only_to(
                &sink,
                CAPTURE_NAME
            )),
            "the capture reads the sink's monitor: {:?}",
            links_from(&sink)
        );
        assert!(
            output_only_to(&out_sink),
            "the output plays on, never into the sink: {:?}",
            own_links(PLAYBACK_NAME)
        );
        assert_eq!(own_nodes(CAPTURE_NAME).len(), 1, "one capture stream");
        assert_eq!(own_nodes(PLAYBACK_NAME).len(), 1, "one output");
        backend.stop().expect("stop");
        assert_eq!(
            own_nodes(CAPTURE_NAME),
            Vec::<u32>::new(),
            "capture removed"
        );
        assert_eq!(own_nodes(PLAYBACK_NAME).len(), 1, "the output stays");
        backend.close_output().expect("close_output");
        assert!(
            wait_until(Duration::from_secs(2), || own_nodes(PLAYBACK_NAME)
                .is_empty()),
            "the output is removed"
        );
        assert!(events.try_recv().is_err(), "no failure reported");
        assert!(output_events.try_recv().is_err(), "no output failure");
    }

    /// Makes the backend take one of its own sinks as the default output, as when the user
    /// picks the Beatsync sink or the last real device goes away. Only the backend's view
    /// changes: the real default output stays as it is.
    fn fake_own_default(backend: &PipeWireBackend) {
        let value = format!(r#"{{"name":"beatsync-{}-999"}}"#, std::process::id());
        let faked = backend.call(|reply| Command::FakeDefaultSink { value, reply });
        faked.expect("fake default");
    }

    /// The next failure the session reports within 3 s.
    fn next_failure(events: &mut UnboundedReceiver<AudioEvent>) -> Option<String> {
        let mut failure = None;
        wait_until(Duration::from_secs(3), || {
            if let Ok(AudioEvent::Failed(m)) = events.try_recv() {
                failure = Some(m);
            }
            failure.is_some()
        });
        failure
    }

    /// Stops `backend` and checks that `app` (stream `id`) is back on `default` with no
    /// routing entry, and that the sink is gone.
    fn stop_restores(backend: &PipeWireBackend, app: &str, id: u32, default: &str) {
        backend.stop().expect("stop");
        assert!(
            wait_until(Duration::from_secs(3), || linked_only_to(app, default)),
            "back on the default sink: {:?}",
            links_from(app)
        );
        assert_eq!(metadata_line(id, "target.object"), None);
        let pid = std::process::id();
        assert!(
            wait_until(Duration::from_secs(2), || sinks_of(pid).is_empty()),
            "the sink is removed"
        );
    }

    #[test]
    #[ignore = "needs a live PipeWire session"]
    fn a_start_with_our_sink_as_the_default_captures_nothing() {
        // The output follows the default output. With our sink as the default, a capture of
        // it would record the PC's own output and send it to the room.
        let _lock = lock();
        let app = format!("beatsync-test-{}", std::process::id());
        let _tone = play_silence(&app);
        let backend = PipeWireBackend::new().expect("backend");
        wait_listed(&backend, &app);
        let id = node_id(&app).expect("stream node");
        let default = default_sink_name();
        let out_sink = test_sink(&backend);
        let mut output_events = open_test_output(&backend, &out_sink);
        fake_own_default(&backend);

        let (io, mut events) = test_io();
        backend.start(vec![app.clone()], io).expect("start");
        assert_eq!(next_failure(&mut events).as_deref(), Some(NO_LOCAL_OUTPUT));
        let sink = sinks_of(std::process::id()).pop().expect("sink");
        assert!(
            wait_until(Duration::from_secs(3), || linked_only_to(&app, &sink)),
            "the app was routed before the check: {:?}",
            links_from(&app)
        );
        // A capture made anyway would show up by now.
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(own_nodes(CAPTURE_NAME), Vec::<u32>::new(), "no capture");

        stop_restores(&backend, &app, id, &default);
        assert_eq!(own_nodes(PLAYBACK_NAME).len(), 1, "the output stays");
        assert!(output_events.try_recv().is_err(), "no output failure");
        assert!(events.try_recv().is_err(), "one failure only");
        backend.close_output().expect("close_output");
    }

    #[test]
    #[ignore = "needs a live PipeWire session"]
    fn our_sink_as_the_new_default_stops_the_capture_at_once() {
        // The default output changes to our sink while streaming, and the output follows it.
        // The capture must stop now, not when the engine has heard of it and stops the stream.
        let _lock = lock();
        let app = format!("beatsync-test-{}", std::process::id());
        let _tone = play_silence(&app);
        let backend = PipeWireBackend::new().expect("backend");
        wait_listed(&backend, &app);
        let id = node_id(&app).expect("stream node");
        let default = default_sink_name();
        let out_sink = test_sink(&backend);
        let mut output_events = open_test_output(&backend, &out_sink);
        let (io, mut events) = test_io();
        backend.start(vec![app.clone()], io).expect("start");
        let sink = sinks_of(std::process::id()).pop().expect("sink");
        assert!(
            wait_until(Duration::from_secs(3), || {
                linked_only_to(&app, &sink) && own_nodes(CAPTURE_NAME).len() == 1
            }),
            "routed and captured: {:?}",
            links_from(&app)
        );

        fake_own_default(&backend);
        assert_eq!(next_failure(&mut events).as_deref(), Some(NO_LOCAL_OUTPUT));
        assert!(
            wait_until(Duration::from_secs(1), || own_nodes(CAPTURE_NAME)
                .is_empty()),
            "the capture stops before the stream stops"
        );
        // The late state of the dropped capture is no second failure.
        std::thread::sleep(Duration::from_millis(200));
        assert!(events.try_recv().is_err(), "one failure only");
        assert_eq!(own_nodes(PLAYBACK_NAME).len(), 1, "the output stays");

        stop_restores(&backend, &app, id, &default);
        assert!(output_events.try_recv().is_err(), "no output failure");
        backend.close_output().expect("close_output");
    }

    #[test]
    #[ignore = "needs a live PipeWire session"]
    fn the_output_survives_stream_start_and_stop() {
        // The queue track plays through the output while streams start and stop.
        let _lock = lock();
        let backend = PipeWireBackend::new().expect("backend");
        let out_sink = test_sink(&backend);
        let mut output_events = open_test_output(&backend, &out_sink);
        let before = own_nodes(PLAYBACK_NAME);
        assert_eq!(before.len(), 1, "one output");
        for round in 0..2 {
            let (io, mut events) = test_io();
            backend.start(Vec::new(), io).expect("start");
            assert!(
                wait_until(Duration::from_secs(3), || own_nodes(CAPTURE_NAME).len()
                    == 1),
                "round {round}: the capture runs"
            );
            assert_eq!(own_nodes(PLAYBACK_NAME), before, "round {round}: streaming");
            backend.stop().expect("stop");
            assert_eq!(own_nodes(PLAYBACK_NAME), before, "round {round}: stopped");
            assert!(events.try_recv().is_err(), "round {round}: no failure");
        }
        assert!(output_only_to(&out_sink), "{:?}", own_links(PLAYBACK_NAME));
        assert!(output_events.try_recv().is_err(), "no output failure");
        backend.close_output().expect("close_output");
        assert!(
            wait_until(Duration::from_secs(2), || own_nodes(PLAYBACK_NAME)
                .is_empty()),
            "the output is removed"
        );
    }

    #[test]
    #[ignore = "needs a live PipeWire session"]
    fn a_failed_output_leaves_the_session_running() {
        // A lost output must not stop the stream the room hears.
        let _lock = lock();
        let backend = PipeWireBackend::new().expect("backend");
        let out_sink = test_sink(&backend);
        let mut output_events = open_test_output(&backend, &out_sink);
        let (io, mut events) = test_io();
        backend.start(Vec::new(), io).expect("start");
        assert!(
            wait_until(Duration::from_secs(3), || own_nodes(CAPTURE_NAME).len()
                == 1),
            "the capture runs"
        );
        let id = own_nodes(PLAYBACK_NAME)[0].to_string();
        output("pw-cli", &["destroy", &id]);
        let mut message = None;
        wait_until(Duration::from_secs(3), || {
            if let Ok(AudioEvent::OutputFailed(m)) = output_events.try_recv() {
                message = Some(m);
            }
            message.is_some()
        });
        let message = message.expect("the runtime hears that the output stopped");
        println!("output failure: {message}");
        assert!(own_nodes(PLAYBACK_NAME).is_empty(), "the output is closed");
        // A wrongly failed session would report within this time.
        std::thread::sleep(Duration::from_millis(300));
        assert!(events.try_recv().is_err(), "the session goes on");
        assert_eq!(own_nodes(CAPTURE_NAME).len(), 1, "the capture goes on");
        backend.stop().expect("stop");
        backend.close_output().expect("close_output with no output");
    }

    #[test]
    #[ignore = "needs a live PipeWire session"]
    fn reports_a_failed_capture_stream() {
        let _lock = lock();
        let backend = PipeWireBackend::new().expect("backend");
        let (io, mut events) = test_io();
        backend.start(Vec::new(), io).expect("start");
        assert!(
            wait_until(Duration::from_secs(3), || own_nodes(CAPTURE_NAME).len()
                == 1),
            "the capture stream exists"
        );
        let id = own_nodes(CAPTURE_NAME)[0].to_string();
        output("pw-cli", &["destroy", &id]);
        let mut message = None;
        wait_until(Duration::from_secs(3), || {
            if let Ok(AudioEvent::Failed(m)) = events.try_recv() {
                message = Some(m);
            }
            message.is_some()
        });
        let message = message.expect("the runtime hears that the capture stopped");
        assert!(message.starts_with("Audio capture"), "{message}");
        backend.stop().expect("stop");
    }

    #[test]
    #[ignore = "needs a live PipeWire session"]
    fn reports_a_removed_sink() {
        let _lock = lock();
        let backend = PipeWireBackend::new().expect("backend");
        let (io, mut events) = test_io();
        backend.start(Vec::new(), io).expect("start");
        let sink = sinks_of(std::process::id()).pop().expect("sink");
        assert!(
            wait_until(Duration::from_secs(3), || linked_only_to(
                &sink,
                CAPTURE_NAME
            )),
            "the capture reads the sink's monitor: {:?}",
            links_from(&sink)
        );
        // Not `pw-cli destroy`: see `Command::DropSink`.
        let dropped = backend.call(Command::DropSink);
        dropped.expect("drop sink");
        assert!(
            wait_until(Duration::from_secs(3), || node_id(&sink).is_none()),
            "the sink is gone"
        );
        let failed = wait_until(Duration::from_secs(3), || {
            matches!(events.try_recv(), Ok(AudioEvent::Failed(_)))
        });
        assert!(failed, "the runtime hears that the sink is gone");
        backend.stop().expect("stop");
    }
}
