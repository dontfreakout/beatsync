//! The metronome kick: one kick per beat on a fixed grid of server time.

use crate::media::{MediaError, decode_bytes};

/// Time between beats. Beat `n` is at `n * BEAT_INTERVAL_MS` of server time (55 BPM).
pub const BEAT_INTERVAL_MS: f64 = 60_000.0 / 55.0;

const FRAMES_PER_MS: f64 = 48.0;
/// The largest absolute frame position [`Metronome::add`] accepts (2^53).
const MAX_FRAME: f64 = 9_007_199_254_740_992.0;
const KICK_WAV: &[u8] = include_bytes!("../../assets/kick.wav");

/// The kick sound and the beat grid.
pub struct Metronome {
    /// Interleaved stereo at 48 kHz.
    kick: Box<[f32]>,
}

impl Metronome {
    /// Decodes the bundled kick. Call this once, off the real-time thread.
    pub fn kick() -> Result<Metronome, MediaError> {
        let kick = decode_bytes(KICK_WAV, "wav")?;
        Ok(Metronome {
            kick: kick.into_boxed_slice(),
        })
    }

    /// Adds the kick to interleaved stereo. Frame `i` is at `first_frame_server_ms + i / 48` ms.
    /// A kick that starts in one buffer continues in the next. Real-time safe.
    pub fn add(&self, stereo: &mut [f32], first_frame_server_ms: f64) {
        let kick_frames = (self.kick.len() / 2) as i64;
        let frames = (stereo.len() / 2) as i64;
        if kick_frames == 0 || frames == 0 {
            return;
        }
        // Whole-frame positions, so the kick lines up the same way in every buffer.
        let first = (first_frame_server_ms * FRAMES_PER_MS).round();
        // A non-finite or absurd time adds nothing. The bound (about 6000 years) keeps the
        // frame arithmetic below far from an i64 overflow.
        if !first.is_finite() || first.abs() > MAX_FRAME {
            return;
        }
        let first = first as i64;
        let beat_frames = BEAT_INTERVAL_MS * FRAMES_PER_MS;
        let lowest = (((first - kick_frames) as f64 / beat_frames).floor() as i64).max(0);
        let highest = (((first + frames) as f64 / beat_frames).ceil() as i64).max(0);
        for beat in lowest..=highest {
            let start = (beat as f64 * beat_frames).round() as i64;
            let from = start.max(first);
            let to = (start + kick_frames).min(first + frames);
            for frame in from..to {
                let k = 2 * (frame - start) as usize;
                let o = 2 * (frame - first) as usize;
                stereo[o] += self.kick[k];
                stereo[o + 1] += self.kick[k + 1];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peak_threshold(m: &Metronome) -> f32 {
        m.kick.iter().fold(0.0_f32, |p, s| p.max(s.abs())) * 0.01
    }

    fn first_loud_frame(buf: &[f32], threshold: f32) -> Option<usize> {
        buf.chunks(2)
            .position(|f| f[0].abs() > threshold || f[1].abs() > threshold)
    }

    #[test]
    fn the_kick_starts_on_the_beat_sample() {
        let m = Metronome::kick().unwrap();
        assert_eq!(m.kick.len(), 2 * 10_503);
        let threshold = peak_threshold(&m);
        let kick_first = first_loud_frame(&m.kick, threshold).unwrap();
        let beat_ms = 7.0 * BEAT_INTERVAL_MS;
        for lead_ms in [100.0, 33.3, 0.0] {
            let first = beat_ms - lead_ms;
            let mut buf = vec![0.0_f32; 2 * 20_000];
            m.add(&mut buf, first);
            let onset = first_loud_frame(&buf, threshold).unwrap();
            let start = ((beat_ms - first) * 48.0).round() as usize;
            assert_eq!(onset, start + kick_first, "lead {lead_ms} ms");
        }
    }

    #[test]
    fn no_kick_between_beats() {
        let m = Metronome::kick().unwrap();
        let beat_ms = 7.0 * BEAT_INTERVAL_MS;
        let mut buf = vec![0.0_f32; 2 * 28_800];
        m.add(&mut buf, beat_ms + 300.0);
        assert!(buf.iter().all(|s| *s == 0.0));
        // Beat 0 exists, earlier times have no beat.
        let mut before = vec![0.0_f32; 2 * 1000];
        m.add(&mut before, -500.0);
        assert!(before.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn a_time_out_of_range_adds_nothing() {
        // A bad clock value must not panic on the real-time thread or add a stray kick.
        let m = Metronome::kick().unwrap();
        for ms in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 1e300, -1e300] {
            let mut buf = vec![0.0_f32; 2 * 1024];
            m.add(&mut buf, ms);
            assert!(buf.iter().all(|s| *s == 0.0), "{ms}");
        }
    }

    #[test]
    fn a_kick_continues_across_buffers() {
        let m = Metronome::kick().unwrap();
        let first = 1.8e12 + 123.0;
        let first = (first / BEAT_INTERVAL_MS).floor() * BEAT_INTERVAL_MS - 50.0;
        let mut whole = vec![0.0_f32; 2 * 16_000];
        m.add(&mut whole, first);
        let mut split = vec![0.0_f32; 2 * 16_000];
        let mut at = 0usize;
        for frames in [1000usize, 1024, 777, 5000, 8199] {
            let ms = first + at as f64 / 48.0;
            m.add(&mut split[2 * at..2 * (at + frames)], ms);
            at += frames;
        }
        assert_eq!(at, 16_000);
        assert!(whole.iter().any(|s| *s != 0.0));
        assert_eq!(whole, split);
    }
}
