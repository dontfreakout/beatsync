//! One fact about MP3 files that Symphonia reads but does not expose: whether the first frame
//! has a Xing, Info or VBRI tag. Without one, Symphonia estimates the frame count of the track
//! from the file size and the first frames.

use std::io::{Read, Seek, SeekFrom};

/// The bytes searched for the first frame after the ID3v2 tags.
const SCAN_BYTES: u64 = 8 * 1024;
const ID3_HEADER_LEN: u64 = 10;
/// Where Symphonia looks for a VBRI tag: 32 bytes after the frame header.
const VBRI_AT: usize = 4 + 32;

/// Whether the first MPEG frame has a Xing, Info or VBRI tag, by the checks Symphonia makes.
/// `false` when no frame is found.
pub(super) fn has_length_tag<R: Read + Seek + ?Sized>(file: &mut R) -> bool {
    first_frame(file).is_some_and(|frame| has_tag(&frame))
}

/// The bytes from the first frame header on, after any ID3v2 tags.
fn first_frame<R: Read + Seek + ?Sized>(file: &mut R) -> Option<Vec<u8>> {
    let mut start = 0u64;
    loop {
        file.seek(SeekFrom::Start(start)).ok()?;
        let mut head = [0u8; ID3_HEADER_LEN as usize];
        if file.read_exact(&mut head).is_err() || &head[..3] != b"ID3" {
            break;
        }
        // The size is "synchsafe": 7 bits per byte. A footer flag adds a 10-byte footer.
        let size = head[6..10]
            .iter()
            .fold(0u64, |size, &b| (size << 7) | u64::from(b & 0x7f));
        let footer = if head[5] & 0x10 != 0 {
            ID3_HEADER_LEN
        } else {
            0
        };
        start += ID3_HEADER_LEN + size + footer;
    }
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(SCAN_BYTES).read_to_end(&mut bytes).ok()?;
    let at = bytes.windows(4).position(is_frame_header)?;
    Some(bytes.split_off(at))
}

fn is_frame_header(header: &[u8]) -> bool {
    let version = (header[1] >> 3) & 0b11;
    let layer = (header[1] >> 1) & 0b11;
    let bitrate = header[2] >> 4;
    let rate = (header[2] >> 2) & 0b11;
    header[0] == 0xff
        && header[1] & 0xe0 == 0xe0
        && version != 0b01
        && layer != 0b00
        && bitrate != 0b1111
        && rate != 0b11
}

/// `frame` starts with a frame header. A Xing or Info tag follows the side information, which
/// must be zero; a VBRI tag sits at a fixed place. Both exist in Layer III only.
fn has_tag(frame: &[u8]) -> bool {
    let mpeg1 = (frame[1] >> 3) & 0b11 == 0b11;
    let layer3 = (frame[1] >> 1) & 0b11 == 0b01;
    if !layer3 {
        return false;
    }
    let mono = frame[3] >> 6 == 0b11;
    let side_info = match (mpeg1, mono) {
        (true, true) => 17,
        (true, false) => 32,
        (false, true) => 9,
        (false, false) => 17,
    };
    // A cleared protection bit means a 2-byte CRC follows the header.
    let header_len = if frame[1] & 1 == 0 { 6 } else { 4 };
    let xing_at = 4 + side_info;
    let xing = frame
        .get(xing_at..xing_at + 4)
        .is_some_and(|id| id == b"Xing" || id == b"Info")
        && frame
            .get(header_len..xing_at)
            .is_some_and(|side| side.iter().all(|&b| b == 0));
    let vbri = frame.get(VBRI_AT..VBRI_AT + 4) == Some(b"VBRI");
    xing || vbri
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::media::fixture;

    fn tagged(name: &str) -> bool {
        has_length_tag(&mut std::fs::File::open(fixture(name)).unwrap())
    }

    #[test]
    fn the_xing_tag_is_found_after_the_id3_tag() {
        assert!(tagged("click_left.mp3"));
        assert!(tagged("truncated.mp3"));
        assert!(!tagged("quiet_start_noxing.mp3"));
    }

    #[test]
    fn short_or_broken_input_has_no_tag() {
        let mut huge_id3 = b"ID3\x04\x00\x00\x7f\x7f\x7f\x7f".to_vec();
        huge_id3.extend_from_slice(&[0xff, 0xfb, 0x90, 0x64]);
        for bytes in [
            &b""[..],
            b"ID3",
            &[0xff, 0xfb],
            &[0xff, 0xfb, 0x90, 0x64, 0, 0],
            &huge_id3,
            b"this is not audio at all",
        ] {
            assert!(!has_length_tag(&mut Cursor::new(bytes)), "{bytes:?}");
        }
    }
}
