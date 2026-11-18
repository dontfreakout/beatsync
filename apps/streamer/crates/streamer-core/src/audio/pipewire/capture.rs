//! Capture of the sink's monitor (spec §9.4): F32 stereo 48 kHz, cut into [`CapturedChunk`]s
//! that carry the CLOCK_MONOTONIC time of their first frame, pushed into a lock-free ring.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use ::pipewire as pw;
use pw::buffer::Buffer;
use pw::core::CoreRc;
use pw::properties::{PropertiesBox, properties};
use pw::spa::utils::Direction;
use pw::stream::{Stream, StreamState};

use super::OWN_APP_NAME;
use super::media::{self, LATENCY, MediaStream, STRIDE, StreamSpec, Timing, frames_to_ns};
use crate::audio::{CHUNK_MAX_FRAMES, CapturedChunk, PlayoutShared};

pub(super) const CAPTURE_NAME: &str = "beatsync-capture";

/// Most buffers one callback takes; normally there is exactly one per cycle.
const MAX_HELD: usize = 8;

pub(super) type CaptureStream = MediaStream<CaptureState>;

/// The real-time state of the capture callback.
pub(super) struct CaptureState {
    tx: rtrb::Producer<CapturedChunk>,
    shared: Arc<PlayoutShared>,
    /// Filled in place, then copied into the ring (keeps 8 KiB off the real-time stack).
    chunk: Box<CapturedChunk>,
    /// Audio was lost since the last chunk that went into the ring.
    lost: bool,
}

fn capture_properties(target: &str) -> PropertiesBox {
    properties! {
        // Without it, PipeWire names the node after the application.
        *pw::keys::NODE_NAME => CAPTURE_NAME,
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::STREAM_CAPTURE_SINK => "true",
        *pw::keys::TARGET_OBJECT => target,
        // Never fall back to a microphone or another sink if the target is missing.
        "node.dont-fallback" => "true",
        *pw::keys::NODE_LATENCY => LATENCY,
        // Excluded from the app list and from routing.
        *pw::keys::APP_NAME => OWN_APP_NAME,
        // WirePlumber stores nothing for it, so it never restores a stale per-run target.
        "state.restore-props" => "false",
        "state.restore-target" => "false",
    }
}

/// Captures the monitor of the sink with `node.name` = `target` into `tx`. Full ring: the
/// chunk is dropped and counted in `shared.capture_dropped`, and the next chunk is marked
/// [`CapturedChunk::after_loss`]. The timestamps assume that the
/// sink drives its graph, as a null sink does (see [`buffer_starts`]).
pub(super) fn connect_capture(
    core: &CoreRc,
    target: &str,
    tx: rtrb::Producer<CapturedChunk>,
    shared: Arc<PlayoutShared>,
    id: u64,
    on_state: impl FnMut(StreamState) + 'static,
) -> Result<CaptureStream, String> {
    let spec = StreamSpec {
        name: CAPTURE_NAME,
        properties: capture_properties(target),
        direction: Direction::Input,
        id,
    };
    let state = CaptureState {
        tx,
        shared,
        chunk: Box::new(CapturedChunk {
            mono_ns_first_frame: 0,
            frames: 0,
            samples: [0.0; CHUNK_MAX_FRAMES * 2],
            after_loss: false,
        }),
        lost: false,
    };
    media::connect(core, spec, state, process, on_state)
}

/// CLOCK_MONOTONIC ns of the cycle position `ticks` of the cycle `t`, less the delay from the
/// device and the frames still in the resampler: `now + (position − ticks)·1e9/rate` with
/// `position = ticks − delay`. The frames of the cycle before end here (see [`buffer_starts`]).
pub(super) fn capture_end_ns(t: &Timing) -> i64 {
    let position_minus_ticks = -(t.delay as f64);
    let offset = position_minus_ticks * t.tick_ns() - frames_to_ns(t.buffered);
    t.now_ns + offset.round() as i64
}

/// Start times of the buffers taken in one callback, oldest first, for any buffer size.
///
/// The sink drives its graph, and the followers of a driver read its output (the monitor) one
/// cycle after the driver produced it (PipeWire `impl-node.c`, `node_ready`: "move output with
/// previous cycle"). So the newest buffer holds the frames the sink took in the cycle before
/// and ends at `end_ns`, the position of this cycle; each earlier buffer ends where the next
/// one starts. The loopback test measured this: one quantum late without it.
pub(super) fn buffer_starts(end_ns: i64, frames: &[usize], starts: &mut [i64]) {
    let mut back: usize = frames.iter().sum();
    for (start, &n) in starts.iter_mut().zip(frames) {
        *start = end_ns - frames_to_ns(back as u64).round() as i64;
        back -= n;
    }
}

/// Cuts interleaved F32LE stereo `bytes` that start at `first_ns` into chunks of at most
/// [`CHUNK_MAX_FRAMES`] frames and hands each to `push`. Real-time safe.
pub(super) fn emit_chunks(
    bytes: &[u8],
    first_ns: i64,
    chunk: &mut CapturedChunk,
    mut push: impl FnMut(&mut CapturedChunk),
) {
    let whole = bytes.len() / STRIDE * STRIDE;
    for (index, part) in bytes[..whole].chunks(CHUNK_MAX_FRAMES * STRIDE).enumerate() {
        let frames = part.len() / STRIDE;
        let offset = frames_to_ns((index * CHUNK_MAX_FRAMES) as u64);
        chunk.mono_ns_first_frame = first_ns + offset.round() as i64;
        chunk.frames = frames;
        let (samples, rest) = chunk.samples.split_at_mut(frames * 2);
        for (sample, le) in samples.iter_mut().zip(part.as_chunks::<4>().0) {
            *sample = f32::from_le_bytes(*le);
        }
        rest.fill(0.0);
        push(chunk);
    }
}

/// Pushes `chunk` into the ring and marks it when audio was lost before it. A full ring drops
/// the chunk and counts it. Real-time safe.
fn push_chunk(
    tx: &mut rtrb::Producer<CapturedChunk>,
    shared: &PlayoutShared,
    lost: &mut bool,
    chunk: &mut CapturedChunk,
) {
    chunk.after_loss = *lost;
    *lost = tx.push(*chunk).is_err();
    if *lost {
        shared.capture_dropped.fetch_add(1, Ordering::Relaxed);
    }
}

/// The valid bytes of a buffer's first data plane.
fn valid_bytes<'b>(buffer: &'b mut Buffer<'_>) -> &'b [u8] {
    let Some(data) = buffer.datas_mut().first_mut() else {
        return &[];
    };
    let offset = data.chunk().offset() as usize;
    let size = data.chunk().size() as usize;
    let Some(bytes) = data.data() else {
        return &[];
    };
    let end = offset.saturating_add(size).min(bytes.len());
    &bytes[offset.min(end)..end]
}

/// The real-time capture callback: takes every waiting buffer, stamps it and pushes chunks.
fn process(stream: &Stream, st: &mut CaptureState) {
    let timing = Timing::of(stream);
    let mut held: [Option<Buffer<'_>>; MAX_HELD] = [const { None }; MAX_HELD];
    let mut frames = [0_usize; MAX_HELD];
    let mut count = 0;
    while count < MAX_HELD {
        let Some(mut buffer) = stream.dequeue_buffer() else {
            break;
        };
        frames[count] = valid_bytes(&mut buffer).len() / STRIDE;
        held[count] = Some(buffer);
        count += 1;
    }
    let Some(timing) = timing else {
        // Not streaming: no time for these frames.
        let chunks: usize = frames[..count]
            .iter()
            .map(|f| f.div_ceil(CHUNK_MAX_FRAMES))
            .sum();
        st.shared
            .capture_dropped
            .fetch_add(chunks as u64, Ordering::Relaxed);
        st.lost |= chunks > 0;
        return;
    };
    let mut starts = [0_i64; MAX_HELD];
    buffer_starts(
        capture_end_ns(&timing),
        &frames[..count],
        &mut starts[..count],
    );
    let CaptureState {
        tx,
        shared,
        chunk,
        lost,
    } = st;
    for (buffer, &start) in held[..count].iter_mut().zip(&starts[..count]) {
        let Some(buffer) = buffer else { continue };
        emit_chunks(valid_bytes(buffer), start, chunk, |c| {
            push_chunk(tx, shared, lost, c)
        });
    }
    // Dropping `held` gives the buffers back to the stream.
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: i64 = 1_000_000;

    fn timing(delay: i64, buffered: u64, rate_denom: u32) -> Timing {
        Timing {
            now_ns: 5_000 * MS,
            rate_num: 1,
            rate_denom,
            delay,
            buffered,
        }
    }

    #[test]
    fn capture_time_is_cycle_time_minus_delay_and_resampler_frames() {
        // A sign error is silent on the null sink (delay 0) and shifts every stamp elsewhere.
        // 480 ticks at 1/48000 = 10 ms; 48 frames in the resampler = 1 ms.
        assert_eq!(capture_end_ns(&timing(480, 48, 48_000)), 4_989 * MS);
        // The delay is in graph ticks: at 1/44100, 441 ticks = 10 ms.
        assert_eq!(capture_end_ns(&timing(441, 0, 44_100)), 4_990 * MS);
    }

    #[test]
    fn buffers_hold_the_previous_cycle_and_end_at_this_one() {
        // A driver's monitor reaches the capture one cycle late: the newest buffer ends at the
        // cycle position, whatever its size; earlier buffers end where the next one starts.
        let t = 1_000 * MS;
        let ns = |frames: u64| frames_to_ns(frames).round() as i64;
        let mut one = [0];
        buffer_starts(t, &[512], &mut one);
        assert_eq!(one, [t - ns(512)]);
        let mut starts = [0; 3];
        buffer_starts(t, &[512, 256, 1024], &mut starts);
        assert_eq!(starts, [t - ns(1792), t - ns(1280), t - ns(1024)]);
    }

    #[test]
    fn long_buffers_are_split_with_advanced_timestamps() {
        // A 2500-frame quantum: 1024 + 1024 + 452 frames, each stamped at its own first frame.
        let frames = 2500;
        let bytes: Vec<u8> = (0..frames * 2)
            .flat_map(|i| (i as f32).to_le_bytes())
            .collect();
        let mut chunk = CapturedChunk {
            mono_ns_first_frame: 0,
            frames: 0,
            samples: [7.0; CHUNK_MAX_FRAMES * 2],
            after_loss: false,
        };
        let mut got = Vec::new();
        emit_chunks(&bytes, 1_000 * MS, &mut chunk, |c| got.push(*c));
        assert!(got.iter().all(|c| !c.after_loss));
        let sizes: Vec<usize> = got.iter().map(|c| c.frames).collect();
        assert_eq!(sizes, [1024, 1024, 452]);
        for (k, c) in got.iter().enumerate() {
            let want = 1_000 * MS + frames_to_ns(k as u64 * 1024).round() as i64;
            assert_eq!(c.mono_ns_first_frame, want, "chunk {k}");
            let first_sample = (k * 1024 * 2) as f32;
            assert_eq!(c.samples[0], first_sample, "chunk {k}");
            assert_eq!(
                c.samples[c.frames * 2 - 1],
                first_sample + (c.frames * 2 - 1) as f32
            );
        }
        assert!(got[2].samples[452 * 2..].iter().all(|&s| s == 0.0));
    }

    #[test]
    fn the_chunk_after_a_full_ring_is_marked() {
        // A dropped 1024-frame chunk is a 21 ms hole, too short for the worker's 50 ms time
        // check; unmarked, the next block would join audio across the hole.
        let (mut tx, mut rx) = rtrb::RingBuffer::<CapturedChunk>::new(1);
        let shared = PlayoutShared::default();
        let mut lost = false;
        let mut chunk = CapturedChunk {
            mono_ns_first_frame: 0,
            frames: 1024,
            samples: [0.0; CHUNK_MAX_FRAMES * 2],
            after_loss: false,
        };
        push_chunk(&mut tx, &shared, &mut lost, &mut chunk);
        push_chunk(&mut tx, &shared, &mut lost, &mut chunk); // full: dropped
        assert_eq!(shared.capture_dropped.load(Ordering::Relaxed), 1);
        assert!(!rx.pop().unwrap().after_loss);
        push_chunk(&mut tx, &shared, &mut lost, &mut chunk);
        assert!(rx.pop().unwrap().after_loss, "the chunk after the drop");
        push_chunk(&mut tx, &shared, &mut lost, &mut chunk);
        assert!(
            !rx.pop().unwrap().after_loss,
            "the mark is for one chunk only"
        );
    }
}
