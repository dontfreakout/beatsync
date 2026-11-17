//! Opus frame encoder: one 20 ms block of interleaved f32 PCM in, one Opus packet out.

use std::fmt;

use opus::{Application, Bitrate, Channels, Encoder};

use crate::frame::{FRAME_SAMPLES, SAMPLE_RATE};

/// Maximum size of one Opus packet in bytes.
const MAX_PACKET_BYTES: usize = 1275;
const BITRATE_BPS: i32 = 128_000;

#[derive(Debug)]
pub enum EncoderError {
    /// `channels` is not 1 or 2.
    UnsupportedChannels(u8),
    /// The input block does not hold exactly `FRAME_SAMPLES × channels` samples.
    WrongLength {
        expected: usize,
        actual: usize,
    },
    Opus(opus::Error),
}

impl fmt::Display for EncoderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedChannels(n) => write!(f, "unsupported channel count {n}"),
            Self::WrongLength { expected, actual } => {
                write!(
                    f,
                    "wrong block length: expected {expected} samples, got {actual}"
                )
            }
            Self::Opus(e) => write!(f, "opus error: {e}"),
        }
    }
}

impl std::error::Error for EncoderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Opus(e) => Some(e),
            _ => None,
        }
    }
}

impl From<opus::Error> for EncoderError {
    fn from(e: opus::Error) -> Self {
        Self::Opus(e)
    }
}

pub struct OpusFrameEncoder {
    encoder: Encoder,
    channels: usize,
    lookahead: u32,
}

impl OpusFrameEncoder {
    pub fn new(channels: u8) -> Result<Self, EncoderError> {
        let layout = match channels {
            1 => Channels::Mono,
            2 => Channels::Stereo,
            n => return Err(EncoderError::UnsupportedChannels(n)),
        };
        let mut encoder = Encoder::new(SAMPLE_RATE, layout, Application::Audio)?;
        encoder.set_bitrate(Bitrate::Bits(BITRATE_BPS))?;
        let lookahead = encoder.get_lookahead()?.max(0) as u32;
        Ok(Self {
            encoder,
            channels: usize::from(channels),
            lookahead,
        })
    }

    /// Encodes exactly `FRAME_SAMPLES × channels` interleaved samples into one Opus packet.
    pub fn encode(&mut self, interleaved: &[f32]) -> Result<Vec<u8>, EncoderError> {
        let expected = FRAME_SAMPLES * self.channels;
        if interleaved.len() != expected {
            return Err(EncoderError::WrongLength {
                expected,
                actual: interleaved.len(),
            });
        }
        let mut out = vec![0u8; MAX_PACKET_BYTES];
        let len = self.encoder.encode_float(interleaved, &mut out)?;
        out.truncate(len);
        Ok(out)
    }

    /// Encoder algorithmic delay in samples at 48 kHz.
    pub fn lookahead_samples(&self) -> u32 {
        self.lookahead
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opus::Decoder;

    fn sine_block(start: usize) -> Vec<f32> {
        (0..FRAME_SAMPLES)
            .flat_map(|i| {
                let t = (start + i) as f32 / SAMPLE_RATE as f32;
                let s = 0.5 * (2.0 * std::f32::consts::PI * 1000.0 * t).sin();
                [s, s]
            })
            .collect()
    }

    #[test]
    fn lookahead_is_312_at_48k() {
        let enc = OpusFrameEncoder::new(2).unwrap();
        assert_eq!(enc.lookahead_samples(), 312);
    }

    #[test]
    fn sine_round_trip_decodes_960_frames() {
        let mut enc = OpusFrameEncoder::new(2).unwrap();
        let mut dec = Decoder::new(SAMPLE_RATE, Channels::Stereo).unwrap();
        let mut pcm = vec![0f32; FRAME_SAMPLES * 2];
        let mut peak = 0f32;
        for n in 0..6 {
            let packet = enc.encode(&sine_block(n * FRAME_SAMPLES)).unwrap();
            assert!(!packet.is_empty() && packet.len() <= MAX_PACKET_BYTES);
            let got = dec.decode_float(&packet, &mut pcm, false).unwrap();
            assert_eq!(got, FRAME_SAMPLES);
            if n >= 3 {
                peak = pcm.iter().fold(peak, |p, s| p.max(s.abs()));
            }
        }
        assert!(peak > 0.1, "decoded signal is silent, peak {peak}");
    }

    #[test]
    fn wrong_length_is_an_error() {
        let mut enc = OpusFrameEncoder::new(2).unwrap();
        assert!(matches!(
            enc.encode(&vec![0.0; FRAME_SAMPLES]),
            Err(EncoderError::WrongLength {
                expected: 1920,
                actual: 960
            })
        ));
        assert!(matches!(
            OpusFrameEncoder::new(3),
            Err(EncoderError::UnsupportedChannels(3))
        ));
    }

    #[test]
    fn encoder_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<OpusFrameEncoder>();
    }
}
