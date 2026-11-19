//! The queue track's feeder: a thread that reads the decoded track one 20 ms block at a time
//! and queues the blocks, each with the exact server time of its first frame, for the
//! output's track slot. Only this thread reads the decoded files.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::audio::PlayoutShared;
use crate::clock::mono_ns;
use crate::frame::{FRAME_DURATION_MS, FRAME_SAMPLES, SAMPLE_RATE};
use crate::media::PcmFile;
use crate::mixer::{FADE_FRAMES, FLUSH_WAIT_MS, Slot, request_flush, wait_flushed};
use crate::playout::{BLOCK_SAMPLES, CHANNELS, PcmBlock};

/// The feeder queues blocks whose target is less than this far ahead of the output's read
/// position ([`read_position_ms`]).
pub const FEED_AHEAD_MS: f64 = 300.0;
/// How often the feeder looks for due blocks, a full ring and a closed command channel.
pub const FEED_POLL_MS: u64 = 5;
/// A start closer than this to the output's read position (or before it) begins at the
/// first block at least this far ahead, so the output gets the block before it reads it.
pub const START_MARGIN_MS: f64 = 40.0;
/// A render-ahead beyond this is taken as this: no output delay is that long.
const MAX_RENDER_AHEAD_MS: f64 = 1000.0;

const FRAMES_PER_MS: f64 = SAMPLE_RATE as f64 / 1000.0;
/// The length of the fade-out at a stop.
const FADE_MS: f64 = FADE_FRAMES as f64 / FRAMES_PER_MS;

/// The server time of the frames the output reads now: the server time, plus the nudge
/// (the mixer plays every target the nudge earlier), plus how far ahead the output renders
/// what it plays ([`PlayoutShared::render_ahead_ms`]).
pub fn read_position_ms(shared: &PlayoutShared, mono_ns: i64) -> f64 {
    let nudge = shared.nudge_ms();
    let nudge = if nudge.is_finite() { nudge } else { 0.0 };
    let ahead = f64::from_bits(shared.render_ahead_ms.load(Ordering::Relaxed));
    let ahead = if ahead.is_nan() {
        0.0
    } else {
        ahead.clamp(0.0, MAX_RENDER_AHEAD_MS)
    };
    shared.mono_to_server_ms(mono_ns) + nudge + ahead
}

/// The blocks of one play of a decoded track: block `k` is due at `start + 20·k` ms and holds
/// the frames from `position·48000 + 960·k` on. Pure: no thread, no clock.
pub struct Feed {
    pcm: Arc<PcmFile>,
    start_server_ms: f64,
    /// The file frame heard at `start_server_ms`.
    first_frame: i64,
    /// The index of the next block.
    next: i64,
    /// No block went out yet: the next one fades in, and a late start skips ahead first.
    first_block: bool,
    stop: Option<Stop>,
    /// The last block went out, or the file could not be read.
    done: bool,
}

impl Feed {
    /// Plays the track from `position_s` (a negative position plays from the start), with
    /// that frame heard at the server time `start_server_ms`.
    pub fn new(pcm: Arc<PcmFile>, start_server_ms: f64, position_s: f64) -> Self {
        // `as` saturates, so a position past any file plays nothing.
        let first_frame = (position_s.max(0.0) * f64::from(SAMPLE_RATE)).round() as i64;
        Self {
            pcm,
            start_server_ms,
            first_frame,
            next: 0,
            first_block: true,
            stop: None,
            done: false,
        }
    }

    /// Ends the track at the server time `server_ms`: the sound fades out over
    /// [`FADE_FRAMES`] and is silent from that time on, and no later block follows. Of two
    /// stops the earlier one counts. Blocks that went out already are not changed.
    pub fn stop_at(&mut self, server_ms: f64) {
        self.set_stop(server_ms, FADE_FRAMES);
    }

    /// [`Self::stop_at`] without the fade, for an output that fades the track out itself.
    pub fn end_at(&mut self, server_ms: f64) {
        self.set_stop(server_ms, 1);
    }

    fn set_stop(&mut self, server_ms: f64, ramp_frames: usize) {
        let frame = ((server_ms - self.start_server_ms) * FRAMES_PER_MS).round() as i64;
        if self.stop.is_none_or(|stop| frame < stop.frame) {
            self.stop = Some(Stop { frame, ramp_frames });
        }
    }

    /// The next block, if its target is less than [`FEED_AHEAD_MS`] after `server_now_ms`,
    /// the output's read position ([`read_position_ms`]). The first block fades in over
    /// [`FADE_FRAMES`]; if the start is less than [`START_MARGIN_MS`] after the read position,
    /// it is the first block at least that far after it. The block
    /// that reaches the end of the file is padded with silence and is the last one. A read
    /// error ends the feed (the track goes silent) and is logged.
    pub fn next_block(&mut self, server_now_ms: f64) -> Option<PcmBlock> {
        if self.done {
            return None;
        }
        if self.first_block {
            let earliest = server_now_ms + START_MARGIN_MS;
            if self.target(self.next) < earliest {
                self.next = ((earliest - self.start_server_ms) / FRAME_DURATION_MS).ceil() as i64;
            }
        }
        let target = self.target(self.next);
        if target >= server_now_ms + FEED_AHEAD_MS {
            return None;
        }
        let offset = self.next.saturating_mul(FRAME_SAMPLES as i64);
        if self.stop.is_some_and(|stop| offset >= stop.frame) {
            self.done = true;
            return None;
        }
        let mut samples = [0.0; BLOCK_SAMPLES];
        let first = self.first_frame.saturating_add(offset).max(0) as u64;
        let read = match self.pcm.read_frames(first, &mut samples) {
            Ok(read) => read,
            Err(e) => {
                eprintln!("streamer: cannot read the decoded track: {e}");
                self.done = true;
                return None;
            }
        };
        if read == 0 {
            self.done = true;
            return None;
        }
        // Past the end, `read_frames` gave silence: this is the last block.
        self.done = read < FRAME_SAMPLES;
        let frames = samples.as_chunks_mut::<CHANNELS>().0;
        if self.first_block {
            for (i, frame) in frames.iter_mut().take(FADE_FRAMES).enumerate() {
                let gain = i as f32 / FADE_FRAMES as f32;
                frame.iter_mut().for_each(|sample| *sample *= gain);
            }
            self.first_block = false;
        }
        if let Some(Stop {
            frame: stop,
            ramp_frames,
        }) = self.stop
        {
            for (i, frame) in frames.iter_mut().enumerate() {
                let left = stop.saturating_sub(offset + i as i64);
                if left < ramp_frames as i64 {
                    let gain = left.max(0) as f32 / ramp_frames as f32;
                    frame.iter_mut().for_each(|sample| *sample *= gain);
                }
            }
            if offset + FRAME_SAMPLES as i64 >= stop {
                self.done = true;
            }
        }
        self.next += 1;
        Some(PcmBlock {
            target_start_server_ms: target,
            samples,
        })
    }

    /// No block follows.
    pub fn is_done(&self) -> bool {
        self.done
    }

    fn target(&self, block: i64) -> f64 {
        self.start_server_ms + FRAME_DURATION_MS * block as f64
    }
}

/// The end of a feed.
#[derive(Clone, Copy)]
struct Stop {
    /// The frame (counted from the feed's first frame) heard at the stop time; silent from
    /// there on.
    frame: i64,
    /// The fade before it, in frames (1: none).
    ramp_frames: usize,
}

/// A command for the feeder thread.
pub enum FeedCmd {
    /// Replaces the track at once: what is queued fades out and is dropped, then this play
    /// is fed.
    Play {
        pcm: Arc<PcmFile>,
        start_server_ms: f64,
        position_s: f64,
    },
    /// Ends the track at the server time, with a fade that ends then (in the output, if the
    /// blocks around that time are queued already).
    StopAt { server_ms: f64 },
    /// Ends the track now: what is queued fades out and is dropped.
    StopNow,
}

/// Starts the feeder thread `streamer-track`. Once the server offset is known it queues the
/// blocks of the current play into `tx`, the output's track slot, keeping them
/// [`FEED_AHEAD_MS`] ahead of the output's read position. It ends when the sender of `cmds`
/// drops: within [`FEED_POLL_MS`], or, while a track is queued, after that track has faded
/// out (at most [`FLUSH_WAIT_MS`]).
pub fn spawn_feeder(
    tx: rtrb::Producer<PcmBlock>,
    shared: Arc<PlayoutShared>,
    offset_known: Arc<AtomicBool>,
    cmds: Receiver<FeedCmd>,
) -> io::Result<JoinHandle<()>> {
    let feeder = Feeder {
        tx,
        shared,
        offset_known,
        feed: None,
        pending: None,
        fed_until_ms: None,
        flush_timeout_logged: false,
    };
    std::thread::Builder::new()
        .name("streamer-track".to_string())
        .spawn(move || feeder.run(&cmds))
}

struct Feeder {
    tx: rtrb::Producer<PcmBlock>,
    shared: Arc<PlayoutShared>,
    offset_known: Arc<AtomicBool>,
    feed: Option<Feed>,
    /// A block that the full ring did not take; it goes first.
    pending: Option<PcmBlock>,
    /// The server time until which blocks taken from a feed play: the end of the last one, or
    /// an earlier stop time the output applies. `None` after a flush.
    fed_until_ms: Option<f64>,
    /// A flush that timed out was logged.
    flush_timeout_logged: bool,
}

impl Feeder {
    fn run(mut self, cmds: &Receiver<FeedCmd>) {
        let poll = Duration::from_millis(FEED_POLL_MS);
        loop {
            // With nothing to feed, wait for the next command (a dropped sender ends the
            // wait at once).
            let idle = self.feed.is_none() && self.pending.is_none();
            let mut next = if idle {
                cmds.recv().map_err(|_| RecvTimeoutError::Disconnected)
            } else {
                cmds.recv_timeout(poll)
            };
            // Every queued command applies before the next block.
            loop {
                match next {
                    Ok(cmd) => self.apply(cmd),
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => {
                        self.stop_now();
                        return;
                    }
                }
                next = cmds.recv_timeout(Duration::ZERO);
            }
            self.fill();
        }
    }

    fn apply(&mut self, cmd: FeedCmd) {
        match cmd {
            FeedCmd::Play {
                pcm,
                start_server_ms,
                position_s,
            } => {
                self.stop_now();
                self.feed = Some(Feed::new(pcm, start_server_ms, position_s));
            }
            FeedCmd::StopAt { server_ms } => match self.fed_until_ms {
                // The fade must end at the stop. If it would start in a block taken already,
                // the output stops the slot itself, exactly at that time (or fades out at
                // once if the time has passed), and the feed ends there without a fade.
                Some(end) if server_ms - FADE_MS < end => {
                    let track = Slot::Track as usize;
                    self.shared.stop_at_ms[track].store(server_ms.to_bits(), Ordering::Release);
                    request_flush(&self.shared, Slot::Track);
                    if let Some(feed) = &mut self.feed {
                        feed.end_at(server_ms);
                    }
                    self.fed_until_ms = Some(end.min(server_ms));
                }
                _ => {
                    if let Some(feed) = &mut self.feed {
                        feed.stop_at(server_ms);
                    }
                }
            },
            FeedCmd::StopNow => self.stop_now(),
        }
    }

    /// Drops the feed, and fades out and drops its blocks that are still queued. A flush the
    /// output has not done yet (a stop time it plays on to) is replaced by one at once, so no
    /// later block is dropped by it.
    fn stop_now(&mut self) {
        self.feed = None;
        self.pending = None;
        let now = self.read_position_ms();
        let track = Slot::Track as usize;
        let flushing = self.shared.flush_requested[track].load(Ordering::Acquire)
            != self.shared.flush_done[track].load(Ordering::Acquire);
        if self.fed_until_ms.take().is_some_and(|end| end > now) || flushing {
            self.flush();
        }
    }

    /// Waits until the output has faded out and dropped the track slot, at once: a stop time
    /// the output still plays on to is cleared first. A timeout (no output runs) is logged
    /// once; the output then flushes when it runs again.
    fn flush(&mut self) {
        let track = Slot::Track as usize;
        self.shared.stop_at_ms[track].store(f64::NAN.to_bits(), Ordering::Release);
        let seq = request_flush(&self.shared, Slot::Track);
        let wait = Duration::from_millis(FLUSH_WAIT_MS);
        if !wait_flushed(&self.shared, Slot::Track, seq, wait) && !self.flush_timeout_logged {
            self.flush_timeout_logged = true;
            eprintln!(
                "streamer: the audio output did not drop the queue track within {FLUSH_WAIT_MS} ms"
            );
        }
    }

    /// Queues the due blocks. A full ring (the output does not run) keeps the block for the
    /// next poll.
    fn fill(&mut self) {
        // Server times mean nothing before the server offset is known.
        if !self.offset_known.load(Ordering::Acquire) {
            return;
        }
        let now = self.read_position_ms();
        loop {
            let block = match self.pending.take() {
                Some(block) => block,
                None => {
                    let Some(block) = self.feed.as_mut().and_then(|feed| feed.next_block(now))
                    else {
                        break;
                    };
                    self.fed_until_ms = Some(block.target_start_server_ms + FRAME_DURATION_MS);
                    block
                }
            };
            if let Err(rtrb::PushError::Full(block)) = self.tx.push(block) {
                self.pending = Some(block);
                break;
            }
        }
        // A finished feed lets go of its file.
        if self.feed.as_ref().is_some_and(Feed::is_done) {
            self.feed = None;
        }
    }

    fn read_position_ms(&self) -> f64 {
        read_position_ms(&self.shared, mono_ns())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{PcmFileWriter, PcmSink};
    use crate::mixer::FADE_FRAMES;
    use crate::test_util::TempDir;

    /// A server time on the 20 ms grid.
    const START: f64 = 1_700_000_000_000.0;

    /// A decoded-track file of `frames` frames whose frame `i` is `frame(i)`.
    fn pcm_file(dir: &TempDir, frames: usize, frame: impl Fn(usize) -> [f32; 2]) -> Arc<PcmFile> {
        let path = dir.0.join("track.pcm");
        let mut writer = PcmFileWriter::create(&path).unwrap();
        let samples: Vec<f32> = (0..frames).flat_map(frame).collect();
        writer.write(&samples).unwrap();
        writer.finish().unwrap();
        Arc::new(PcmFile::open(&path).unwrap())
    }

    /// Frame `i` holds `i` (modulo 16384) in 16-bit steps, so a read frame tells its index.
    fn counting(i: usize) -> [f32; 2] {
        let v = (i % 16_384) as f32 / 32_768.0;
        [v, -v]
    }

    fn constant(_: usize) -> [f32; 2] {
        [0.5, 0.5]
    }

    /// The file frame (modulo 16384) of frame `i` of a block of a [`counting`] file.
    fn index_of(block: &PcmBlock, i: usize) -> usize {
        (block.samples[2 * i] * 32_768.0).round() as usize
    }

    fn left(block: &PcmBlock, i: usize) -> f32 {
        block.samples[2 * i]
    }

    fn all(feed: &mut Feed, now: f64) -> Vec<PcmBlock> {
        std::iter::from_fn(|| feed.next_block(now)).collect()
    }

    fn targets(blocks: &[PcmBlock]) -> Vec<f64> {
        blocks.iter().map(|b| b.target_start_server_ms).collect()
    }

    #[test]
    fn blocks_carry_exact_targets() {
        // The output resyncs on any step above 0.5 ms between blocks: targets must be exact,
        // and each block must hold the frames heard at its target.
        let dir = TempDir::new("feed-targets");
        let mut feed = Feed::new(pcm_file(&dir, 96_000, counting), START, 0.5);
        assert!(
            feed.next_block(START - FEED_AHEAD_MS).is_none(),
            "not due yet"
        );
        let blocks = all(&mut feed, START - 200.0);
        assert_eq!(
            targets(&blocks),
            [
                START,
                START + 20.0,
                START + 40.0,
                START + 60.0,
                START + 80.0
            ]
        );
        let later = all(&mut feed, START);
        assert_eq!(later.len(), 10);
        for (k, block) in blocks.iter().chain(&later).enumerate() {
            assert_eq!(block.target_start_server_ms, START + 20.0 * k as f64);
            // Past the fade-in of the first block.
            for i in FADE_FRAMES..960 {
                assert_eq!(
                    index_of(block, i),
                    (24_000 + 960 * k + i) % 16_384,
                    "block {k} frame {i}"
                );
            }
        }
    }

    #[test]
    fn a_late_start_skips_to_the_reached_position() {
        // The track decoded after the room started it: the PC joins in where the room is,
        // not at the start position.
        let dir = TempDir::new("feed-late");
        let file = pcm_file(&dir, 192_000, counting);
        let mut feed = Feed::new(file.clone(), START, 1.0);
        // 500 ms in: the first block at least 40 ms ahead is block 27.
        let first = feed.next_block(START + 500.0).expect("a block");
        assert_eq!(first.target_start_server_ms, START + 540.0);
        for i in FADE_FRAMES..960 {
            assert_eq!(index_of(&first, i), (48_000 + 27 * 960 + i) % 16_384);
        }
        let second = feed.next_block(START + 500.0).expect("a block");
        assert_eq!(second.target_start_server_ms, START + 560.0);
        assert_eq!(index_of(&second, 0), (48_000 + 28 * 960) % 16_384);

        // Between two block starts: the later block.
        let mut feed = Feed::new(file, START, 1.0);
        let first = feed.next_block(START + 501.0).expect("a block");
        assert_eq!(first.target_start_server_ms, START + 560.0);
    }

    #[test]
    fn the_first_block_fades_in() {
        // A start in the middle of a loud track must not click.
        let dir = TempDir::new("feed-fade-in");
        let file = pcm_file(&dir, 48_000, constant);
        for now in [START - 100.0, START + 500.0] {
            let mut feed = Feed::new(file.clone(), START, 0.0);
            let first = feed.next_block(now).expect("a block");
            assert_eq!(first.samples[..2], [0.0, 0.0], "now {now}");
            let half = FADE_FRAMES / 2;
            assert!((left(&first, half) - 0.25).abs() < 1e-6, "now {now}");
            assert!(left(&first, FADE_FRAMES - 1) < 0.5, "now {now}");
            assert!(first.samples[2 * FADE_FRAMES..].iter().all(|&s| s == 0.5));
            let second = feed.next_block(now).expect("a block");
            assert!(second.samples.iter().all(|&s| s == 0.5), "now {now}");
        }
    }

    #[test]
    fn stop_at_fades_out_and_ends_at_its_time() {
        // A PAUSE at T: the sound fades over 5 ms and is silent from T on, and nothing after
        // T is queued.
        let dir = TempDir::new("feed-stop");
        let mut feed = Feed::new(pcm_file(&dir, 48_000, constant), START, 0.0);
        feed.stop_at(START + 50.0);
        let blocks = all(&mut feed, START - 100.0);
        assert_eq!(targets(&blocks), [START, START + 20.0, START + 40.0]);
        // T is frame 480 of the last block.
        let last = &blocks[2];
        let fade_start = 480 - FADE_FRAMES;
        assert!(last.samples[..2 * fade_start].iter().all(|&s| s == 0.5));
        assert!((left(last, fade_start + FADE_FRAMES / 2) - 0.25).abs() < 1e-6);
        assert!(left(last, 479) > 0.0);
        assert!(last.samples[2 * 480..].iter().all(|&s| s == 0.0));
        assert!(feed.next_block(START + 10_000.0).is_none());

        // Ended for an output that fades out itself: no fade, silent from T on.
        let mut feed = Feed::new(pcm_file(&dir, 48_000, constant), START, 0.0);
        feed.end_at(START + 50.0);
        let blocks = all(&mut feed, START - 100.0);
        assert_eq!(targets(&blocks), [START, START + 20.0, START + 40.0]);
        assert!(blocks[2].samples[..2 * 480].iter().all(|&s| s == 0.5));
        assert!(blocks[2].samples[2 * 480..].iter().all(|&s| s == 0.0));
    }

    /// Shared values whose server time at the CLOCK_MONOTONIC time [`MONO_NS`] is [`START`],
    /// with the output rendering `render_ahead_ms` ahead.
    fn shared_at_start(render_ahead_ms: f64) -> PlayoutShared {
        let shared = PlayoutShared::default();
        let offset_ms = START - MONO_NS as f64 / 1e6;
        let store = |atomic: &std::sync::atomic::AtomicU64, value: f64| {
            atomic.store(value.to_bits(), Ordering::Relaxed)
        };
        store(&shared.mono_server_offset_ms, offset_ms);
        store(&shared.render_ahead_ms, render_ahead_ms);
        shared
    }

    const MONO_NS: i64 = 5_000_000_000_000;

    #[test]
    fn a_late_start_with_a_large_render_ahead_fades_in_after_the_read_position() {
        // A Bluetooth output renders 250 ms before it plays. A late start that ignored this
        // would begin inside a block the output reads already, without its fade-in (a click).
        let dir = TempDir::new("feed-render-ahead");
        let file = pcm_file(&dir, 96_000, constant);
        let shared = shared_at_start(250.0);
        let read = read_position_ms(&shared, MONO_NS);
        assert_eq!(read, START + 250.0);
        let mut feed = Feed::new(file, START - 100.0, 0.0);
        let first = feed.next_block(read).expect("a block");
        assert_eq!(
            first.target_start_server_ms,
            START + 300.0,
            "the first block at least 40 ms after the read position"
        );
        assert_eq!(first.samples[..2], [0.0, 0.0], "it fades in");
        assert_eq!(first.samples[2 * FADE_FRAMES], 0.5);
    }

    #[test]
    fn the_horizon_grows_with_the_render_ahead() {
        // The output reads blocks the render-ahead before they are heard: the feeder must
        // keep its 300 ms ahead of that, or a large output delay underruns.
        let dir = TempDir::new("feed-horizon");
        let file = pcm_file(&dir, 96_000, constant);
        let queued = |render_ahead_ms: f64| {
            let shared = shared_at_start(render_ahead_ms);
            let mut feed = Feed::new(file.clone(), START + 250.0, 0.0);
            all(&mut feed, read_position_ms(&shared, MONO_NS)).len()
        };
        // Targets below START + 300: blocks START + 250, 270, 290.
        assert_eq!(queued(0.0), 3);
        // Targets below START + 500: blocks START + 250 … START + 490.
        assert_eq!(queued(200.0), 13);
        // A value that is no delay counts as none.
        assert_eq!(queued(f64::NAN), 3);
        assert_eq!(queued(-50.0), 3);
    }

    #[test]
    fn the_track_end_pads_once_and_stops() {
        // The last block is padded with silence; no block of silence follows, so the output
        // underruns at the end of the track.
        let dir = TempDir::new("feed-end");
        let mut feed = Feed::new(pcm_file(&dir, 2_400, constant), START, 0.0);
        let blocks = all(&mut feed, START - 100.0);
        assert_eq!(targets(&blocks), [START, START + 20.0, START + 40.0]);
        let last = &blocks[2];
        assert!(last.samples[..2 * 480].iter().all(|&s| s == 0.5));
        assert!(last.samples[2 * 480..].iter().all(|&s| s == 0.0));
        assert!(feed.next_block(START + 10_000.0).is_none());

        // An end on a block boundary: no block of silence.
        let dir = TempDir::new("feed-end-boundary");
        let mut feed = Feed::new(pcm_file(&dir, 1_920, constant), START, 0.0);
        assert_eq!(all(&mut feed, START - 100.0).len(), 2);
        assert!(feed.next_block(START + 10_000.0).is_none());
    }

    #[test]
    fn a_read_error_ends_the_feed() {
        // A file that can no longer be read must not be retried every few ms.
        let dir = TempDir::new("feed-read-error");
        let file = pcm_file(&dir, 48_000, constant);
        std::fs::OpenOptions::new()
            .write(true)
            .open(dir.0.join("track.pcm"))
            .unwrap()
            .set_len(0)
            .unwrap();
        let mut feed = Feed::new(file, START, 0.0);
        assert!(feed.next_block(START).is_none());
        assert!(feed.next_block(START + 1000.0).is_none());
    }
}
