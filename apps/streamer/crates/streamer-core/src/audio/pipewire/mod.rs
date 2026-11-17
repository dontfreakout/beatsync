//! PipeWire backend (Linux, spec §9.3): needs PipeWire 1.0 and WirePlumber 0.5 or later.

use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ::pipewire as pw;

use super::{AppInfo, AudioBackend, AudioError, AudioIo};

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
/// Routing runs this long after a stream appears, changes or vanishes, or the selection changes.
const ROUTE_DEBOUNCE: Duration = Duration::from_millis(200);
/// Longer than every wait of the thread, so a reply only times out when the thread hangs.
const REPLY_TIMEOUT: Duration = Duration::from_secs(4);

/// The PipeWire backend. All PipeWire objects live on its own thread; the methods block until
/// that thread answers. Dropping it stops routing (if started), removes the sink and ends the
/// thread.
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
    //! They need PipeWire 1.0+, WirePlumber 0.5+ and `pw-cat`, `pw-dump`, `pw-link`,
    //! `pw-metadata`, `pw-cli`. They touch only what they create: their own sinks and
    //! `pw-cat` streams that play silence from `/dev/zero`. Other streams and the default
    //! devices stay as they are. The tests run one at a time (`LOCK`), and every child
    //! process is killed on exit, also when a test fails.

    use std::process::{Child, Command as Process, Stdio};
    use std::sync::{Arc, Mutex, MutexGuard};

    use serde_json::Value;
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    use super::*;
    use crate::audio::{AudioEvent, PlayoutShared};

    static LOCK: Mutex<()> = Mutex::new(());
    /// Set for the child process of `sink_disappears_when_the_owning_connection_closes`.
    const CHILD_ENV: &str = "BEATSYNC_SINK_CHILD";

    fn lock() -> MutexGuard<'static, ()> {
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn test_io() -> (AudioIo, UnboundedReceiver<AudioEvent>) {
        let (capture_tx, _) = rtrb::RingBuffer::new(4);
        let (_, playout_rx) = rtrb::RingBuffer::new(4);
        let (events, events_rx) = unbounded_channel();
        let io = AudioIo {
            capture_tx,
            playout_rx,
            shared: Arc::new(PlayoutShared::default()),
            events,
        };
        (io, events_rx)
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
    fn play_silence(name: &str) -> ChildGuard {
        let props = format!(
            "{{ application.name = \"{name}\" node.name = \"{name}\" \
             state.restore-props = false state.restore-target = false }}"
        );
        let child = Process::new("pw-cat")
            .args(["-p", "--raw", "--rate", "48000", "--channels", "2"])
            .args(["--format", "f32", "--properties", &props, "/dev/zero"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("pw-cat");
        ChildGuard(child)
    }

    fn output(program: &str, args: &[&str]) -> String {
        let out = Process::new(program).args(args).output().expect(program);
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// `(id, node.name)` of every node in `pw-dump`.
    fn nodes() -> Vec<(u64, String)> {
        let dump: Value = serde_json::from_str(&output("pw-dump", &[])).expect("pw-dump JSON");
        dump.as_array()
            .into_iter()
            .flatten()
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
        let dump: Value = serde_json::from_str(&output("pw-dump", &[])).ok()?;
        let node = dump.as_array()?.iter().find(|o| {
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
    fn links_from(name: &str) -> Vec<String> {
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

    fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
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

    #[test]
    #[ignore = "needs a live PipeWire session"]
    fn reports_a_removed_sink() {
        let _lock = lock();
        let backend = PipeWireBackend::new().expect("backend");
        let (io, mut events) = test_io();
        backend.start(Vec::new(), io).expect("start");
        let sink = sinks_of(std::process::id()).pop().expect("sink");
        let id = node_id(&sink).expect("sink node").to_string();
        output("pw-cli", &["destroy", &id]);
        let failed = wait_until(Duration::from_secs(3), || {
            matches!(events.try_recv(), Ok(AudioEvent::Failed(_)))
        });
        assert!(failed, "the runtime hears that the sink is gone");
        backend.stop().expect("stop");
    }
}
