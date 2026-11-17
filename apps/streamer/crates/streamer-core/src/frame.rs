//! Binary live-stream frame codec (little-endian), mirrors `packages/shared/liveFrame.ts`.

pub const SAMPLE_RATE: u32 = 48_000;
pub const FRAME_SAMPLES: usize = 960;
pub const FRAME_DURATION_MS: f64 = 20.0;
pub const HEADER_BYTES: usize = 17;
pub const FRAME_KIND_AUDIO: u8 = 1;
pub const MAX_FRAME_BYTES: usize = 4096;
pub const MIN_DELAY_MS: u32 = 250;
pub const MAX_DELAY_MS: u32 = 5000;
pub const DEFAULT_DELAY_MS: u32 = 1000;
pub const MAX_LABEL_LEN: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameHeader {
    pub stream_id: u32,
    pub sequence: u32,
    pub capture_server_time_ms: f64,
}

pub fn encode_frame(h: &FrameHeader, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_BYTES + payload.len());
    out.push(FRAME_KIND_AUDIO);
    out.extend_from_slice(&h.stream_id.to_le_bytes());
    out.extend_from_slice(&h.sequence.to_le_bytes());
    out.extend_from_slice(&h.capture_server_time_ms.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// Returns `None` for frames that are too short, too long or not audio frames.
pub fn decode_frame(bytes: &[u8]) -> Option<(FrameHeader, &[u8])> {
    if bytes.len() < HEADER_BYTES || bytes.len() > MAX_FRAME_BYTES || bytes[0] != FRAME_KIND_AUDIO {
        return None;
    }
    let stream_id = u32::from_le_bytes(bytes[1..5].try_into().ok()?);
    let sequence = u32::from_le_bytes(bytes[5..9].try_into().ok()?);
    let capture_server_time_ms = f64::from_le_bytes(bytes[9..17].try_into().ok()?);
    Some((
        FrameHeader {
            stream_id,
            sequence,
            capture_server_time_ms,
        },
        &bytes[HEADER_BYTES..],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_rejects_short_and_foreign_frames() {
        let h = FrameHeader {
            stream_id: 1,
            sequence: 2,
            capture_server_time_ms: 3.0,
        };
        let ok = encode_frame(&h, &[9, 9]);
        assert!(decode_frame(&ok).is_some());
        assert!(decode_frame(&ok[..HEADER_BYTES - 1]).is_none());
        assert!(decode_frame(&[]).is_none());
        let mut foreign = ok.clone();
        foreign[0] = 2;
        assert!(decode_frame(&foreign).is_none());
        let big = encode_frame(&h, &vec![0; MAX_FRAME_BYTES - HEADER_BYTES]);
        assert!(decode_frame(&big).is_some());
        let too_big = encode_frame(&h, &vec![0; MAX_FRAME_BYTES - HEADER_BYTES + 1]);
        assert!(decode_frame(&too_big).is_none());
    }
}
