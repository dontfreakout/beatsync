//! Audio effects the web client applies to the room's sound: a low-pass filter and a metronome.

mod lowpass;
mod metronome;

pub use lowpass::{LOW_PASS_MAX_HZ, LOW_PASS_MIN_HZ, LOW_PASS_Q_DB, LowPass, lowpass_coefficients};
pub use metronome::{BEAT_INTERVAL_MS, Metronome};
