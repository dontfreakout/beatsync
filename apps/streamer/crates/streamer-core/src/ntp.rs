//! Port of the web client NTP logic (`apps/client/src/utils/ntp.ts`):
//! probe pairs with gap validation and a min-RTT window.

use std::collections::VecDeque;

pub const PROBE_GAP_MS: f64 = 25.0;
pub const PROBE_GAP_TOLERANCE_MS: f64 = 5.0;
pub const MAX_MEASUREMENTS: usize = 16;
pub const INITIAL_INTERVAL_MS: u64 = 50;
pub const STEADY_INTERVAL_MS: u64 = 2500;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NtpMeasurement {
    pub t0: f64,
    pub t1: f64,
    pub t2: f64,
    pub t3: f64,
    pub rtt_ms: f64,
    pub offset_ms: f64,
}

impl NtpMeasurement {
    pub fn new(t0: f64, t1: f64, t2: f64, t3: f64) -> Self {
        Self {
            t0,
            t1,
            t2,
            t3,
            rtt_ms: (t3 - t0) - (t2 - t1),
            offset_ms: ((t1 - t0) + (t2 - t3)) / 2.0,
        }
    }
}

/// Buffers the first probe of a pair and validates the pair when the second arrives.
#[derive(Debug, Default)]
pub struct ProbePairValidator {
    pending: Option<(u64, NtpMeasurement)>,
}

impl ProbePairValidator {
    /// Returns the lower-RTT probe when a pair of the same group is pure, else `None`.
    pub fn on_response(
        &mut self,
        m: NtpMeasurement,
        group_id: u64,
        index: u8,
    ) -> Option<NtpMeasurement> {
        if index == 0 {
            self.pending = Some((group_id, m));
            return None;
        }
        let (first_group, first) = self.pending.take()?;
        if first_group != group_id {
            return None;
        }
        let client_gap = m.t0 - first.t0;
        let server_gap = m.t1 - first.t1;
        if (server_gap - client_gap).abs() > PROBE_GAP_TOLERANCE_MS {
            return None;
        }
        Some(if first.rtt_ms <= m.rtt_ms { first } else { m })
    }
}

/// Window of the last `MAX_MEASUREMENTS` accepted measurements.
#[derive(Debug, Default)]
pub struct NtpEstimator {
    window: VecDeque<NtpMeasurement>,
    seen: usize,
}

impl NtpEstimator {
    pub fn push(&mut self, m: NtpMeasurement) {
        if self.window.len() == MAX_MEASUREMENTS {
            self.window.pop_front();
        }
        self.window.push_back(m);
        self.seen += 1;
    }

    /// Offset of the min-RTT measurement in the window.
    pub fn offset_ms(&self) -> Option<f64> {
        let mut best: Option<&NtpMeasurement> = None;
        for m in &self.window {
            if best.is_none_or(|b| m.rtt_ms < b.rtt_ms) {
                best = Some(m);
            }
        }
        best.map(|m| m.offset_ms)
    }

    /// Mean RTT over the window.
    pub fn avg_rtt_ms(&self) -> Option<f64> {
        if self.window.is_empty() {
            return None;
        }
        Some(self.window.iter().map(|m| m.rtt_ms).sum::<f64>() / self.window.len() as f64)
    }

    /// Accepted measurements in the window (0..=`MAX_MEASUREMENTS`).
    pub fn measurements(&self) -> usize {
        self.seen.min(MAX_MEASUREMENTS)
    }

    pub fn is_synced(&self) -> bool {
        self.seen >= MAX_MEASUREMENTS
    }

    pub fn reset(&mut self) {
        self.window.clear();
        self.seen = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(t0: f64, t1: f64, t2: f64, t3: f64) -> NtpMeasurement {
        NtpMeasurement::new(t0, t1, t2, t3)
    }

    /// Measurement with a chosen rtt and offset (symmetric legs, no server delay).
    fn with(rtt: f64, offset: f64) -> NtpMeasurement {
        let t0 = 1000.0;
        m(
            t0,
            t0 + rtt / 2.0 + offset,
            t0 + rtt / 2.0 + offset,
            t0 + rtt,
        )
    }

    #[test]
    fn measurement_formula() {
        let x = m(1000.0, 1510.0, 1512.0, 1030.0);
        assert_eq!(x.offset_ms, 496.0);
        assert_eq!(x.rtt_ms, 28.0);
    }

    #[test]
    fn impure_pair_is_rejected() {
        let mut v = ProbePairValidator::default();
        assert!(
            v.on_response(m(1000.0, 1500.0, 1500.0, 1020.0), 7, 0)
                .is_none()
        );
        // client gap 25 ms, server gap 40 ms
        assert!(
            v.on_response(m(1025.0, 1540.0, 1540.0, 1045.0), 7, 1)
                .is_none()
        );
    }

    #[test]
    fn pure_pair_returns_lower_rtt() {
        let mut v = ProbePairValidator::default();
        let first = m(1000.0, 1500.0, 1500.0, 1030.0); // rtt 30
        let second = m(1025.0, 1527.0, 1527.0, 1045.0); // rtt 20
        assert!(v.on_response(first, 3, 0).is_none());
        let best = v.on_response(second, 3, 1).expect("pure pair");
        assert_eq!(best.rtt_ms, 20.0);
        let mut v = ProbePairValidator::default();
        let first = m(1000.0, 1500.0, 1500.0, 1010.0); // rtt 10
        let second = m(1025.0, 1527.0, 1527.0, 1045.0); // rtt 20
        v.on_response(first, 4, 0);
        assert_eq!(v.on_response(second, 4, 1).unwrap().rtt_ms, 10.0);
    }

    #[test]
    fn mismatched_group_is_rejected() {
        let mut v = ProbePairValidator::default();
        v.on_response(m(1000.0, 1500.0, 1500.0, 1020.0), 1, 0);
        assert!(
            v.on_response(m(1025.0, 1525.0, 1525.0, 1045.0), 2, 1)
                .is_none()
        );
        // pending was cleared
        assert!(
            v.on_response(m(1025.0, 1525.0, 1525.0, 1045.0), 1, 1)
                .is_none()
        );
    }

    #[test]
    fn offset_comes_from_min_rtt_measurement() {
        let mut e = NtpEstimator::default();
        assert_eq!(e.offset_ms(), None);
        assert_eq!(e.avg_rtt_ms(), None);
        e.push(with(40.0, 100.0));
        e.push(with(10.0, 120.0));
        e.push(with(30.0, 90.0));
        assert_eq!(e.offset_ms(), Some(120.0));
        assert_eq!(e.avg_rtt_ms(), Some((40.0 + 10.0 + 30.0) / 3.0));
    }

    #[test]
    fn synced_after_16_and_window_rolls() {
        let mut e = NtpEstimator::default();
        e.push(with(5.0, 1.0)); // min RTT, will be evicted
        for _ in 0..14 {
            e.push(with(20.0, 2.0));
        }
        assert!(!e.is_synced());
        e.push(with(20.0, 2.0));
        assert!(e.is_synced());
        assert_eq!(e.offset_ms(), Some(1.0));
        e.push(with(20.0, 2.0)); // 17th push evicts the oldest
        assert_eq!(e.offset_ms(), Some(2.0));
        assert!(e.is_synced());
    }

    #[test]
    fn reset_clears_synced() {
        let mut e = NtpEstimator::default();
        for _ in 0..16 {
            e.push(with(20.0, 2.0));
        }
        assert!(e.is_synced());
        e.reset();
        assert!(!e.is_synced());
        assert_eq!(e.offset_ms(), None);
    }
}
