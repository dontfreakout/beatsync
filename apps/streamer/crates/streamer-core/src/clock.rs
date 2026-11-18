//! Local clocks: a monotonic clock, the wall clock, and a mapping between them.

use std::time::{SystemTime, UNIX_EPOCH};

/// Nanoseconds on `CLOCK_MONOTONIC`.
#[allow(clippy::useless_conversion)] // the timespec fields are i32 on 32-bit targets
pub fn mono_ns() -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable timespec and CLOCK_MONOTONIC is always supported.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    debug_assert_eq!(rc, 0);
    i64::from(ts.tv_sec) * 1_000_000_000 + i64::from(ts.tv_nsec)
}

/// Milliseconds since the Unix epoch, from the system clock.
pub fn epoch_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64() * 1000.0)
}

/// A change of the mono→epoch offset above this between two measurements 1 s apart is a
/// wall-clock step (chrony's or timesyncd's step, a manual change). A kernel slew changes it by
/// at most 0.5 ms per second; a faster change is handled like a step too.
pub const CLOCK_STEP_THRESHOLD_MS: f64 = 2.0;

/// The wall-clock step between two measurements of the mono→epoch offset (epoch − mono, in
/// ms), or `None` when the change is not above [`CLOCK_STEP_THRESHOLD_MS`].
pub fn clock_step_ms(previous_offset_ms: f64, current_offset_ms: f64) -> Option<f64> {
    let delta = current_offset_ms - previous_offset_ms;
    (delta.abs() > CLOCK_STEP_THRESHOLD_MS).then_some(delta)
}

/// A pair (monotonic ns, epoch ms) taken at the same moment.
/// The runtime measures it again every second to follow wall-clock steps.
#[derive(Debug, Clone, Copy)]
pub struct MonoEpochMap {
    mono_ns: i64,
    epoch_ms: f64,
}

impl MonoEpochMap {
    /// Take 16 bracketed reads and keep the one with the smallest monotonic bracket.
    pub fn measure() -> Self {
        let mut best: Option<(i64, Self)> = None;
        for _ in 0..16 {
            let before = mono_ns();
            let epoch = epoch_ms();
            let after = mono_ns();
            let width = after - before;
            if best.is_none_or(|(w, _)| width < w) {
                let mid = before + width / 2;
                best = Some((
                    width,
                    Self {
                        mono_ns: mid,
                        epoch_ms: epoch,
                    },
                ));
            }
        }
        best.expect("16 iterations").1
    }

    /// Convert a `CLOCK_MONOTONIC` time to epoch milliseconds.
    pub fn to_epoch_ms(&self, mono_ns: i64) -> f64 {
        self.epoch_ms + (mono_ns - self.mono_ns) as f64 / 1_000_000.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_map_matches_system_time() {
        let map = MonoEpochMap::measure();
        let diff = (map.to_epoch_ms(mono_ns()) - epoch_ms()).abs();
        assert!(diff < 5.0, "diff was {diff} ms");
    }

    #[test]
    fn only_a_change_above_2_ms_is_a_step() {
        // A slew must not restart NTP every second; a step must shift the server offset.
        assert_eq!(clock_step_ms(1000.0, 1001.9), None);
        assert_eq!(clock_step_ms(1000.0, 998.1), None);
        assert_eq!(clock_step_ms(1000.0, 1002.5), Some(2.5));
        assert_eq!(clock_step_ms(1000.0, 400.0), Some(-600.0));
    }
}
