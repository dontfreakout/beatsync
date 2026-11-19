//! Two facts about AAC tracks that Symphonia reads but does not act on: the MP4 edit list (the
//! encoder delay to drop and the length to keep) and the SBR flag at the end of an
//! AudioSpecificConfig (HE-AAC that Symphonia would decode without its high band).

use std::io::{self, Read, Seek, SeekFrom};
use std::ops::Range;

use symphonia::core::io::{BitReaderLtr, FiniteBitStream, ReadBitsLtr};

/// Only the first entries of an edit list are read.
const ELST_MAX_BYTES: u64 = 4096;
/// The `0x2B7` sync word of the AudioSpecificConfig extension that carries the SBR flag.
const SYNC_EXTENSION: u32 = 0x2B7;
const AOT_AAC_LC: u32 = 2;
const AOT_SBR: u32 = 5;
const AOT_PS: u32 = 29;

/// The edit of one track, in the units of its media timescale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct EditTrim {
    /// Frames to drop at the start (the encoder delay).
    pub skip: u64,
    /// Frames to keep after the dropped ones; `None` keeps the rest.
    pub keep: Option<u64>,
    /// Units per second of `skip` and `keep`.
    pub timescale: u32,
}

impl EditTrim {
    /// `skip` and `keep` in frames at `rate`.
    pub fn at_rate(&self, rate: u32) -> (u64, Option<u64>) {
        let scale = |units: u64| rescale(units, u64::from(rate), u64::from(self.timescale));
        (scale(self.skip), self.keep.map(scale))
    }
}

/// The edit of the track whose `tkhd` id is `track_id` (Symphonia uses that id as the track id).
/// `None` when the track has no edit list or the boxes cannot be read: the track then plays
/// untrimmed, as Symphonia gives it.
pub(super) fn edit_trim<R: Read + Seek + ?Sized>(file: &mut R, track_id: u32) -> Option<EditTrim> {
    read_edit_trim(file, track_id).ok().flatten()
}

fn read_edit_trim<R: Read + Seek + ?Sized>(
    file: &mut R,
    track_id: u32,
) -> io::Result<Option<EditTrim>> {
    let end = file.seek(SeekFrom::End(0))?;
    let Some(moov) = find_box(file, 0..end, b"moov")? else {
        return Ok(None);
    };
    let Some(mvhd) = find_box(file, moov.clone(), b"mvhd")? else {
        return Ok(None);
    };
    let Some(movie_timescale) = u32_after_times(&read_body(file, &mvhd, 32)?) else {
        return Ok(None);
    };
    let mut rest = moov;
    while let Some(trak) = next_box(file, &mut rest)? {
        if &trak.kind != b"trak" {
            continue;
        }
        let Some(tkhd) = find_box(file, trak.body.clone(), b"tkhd")? else {
            continue;
        };
        if u32_after_times(&read_body(file, &tkhd, 32)?) != Some(track_id) {
            continue;
        }
        let Some(edts) = find_box(file, trak.body.clone(), b"edts")? else {
            return Ok(None);
        };
        let Some(elst) = find_box(file, edts, b"elst")? else {
            return Ok(None);
        };
        let Some(mdia) = find_box(file, trak.body.clone(), b"mdia")? else {
            return Ok(None);
        };
        let Some(mdhd) = find_box(file, mdia, b"mdhd")? else {
            return Ok(None);
        };
        let Some(media_timescale) = u32_after_times(&read_body(file, &mdhd, 32)?) else {
            return Ok(None);
        };
        let elst = read_body(file, &elst, ELST_MAX_BYTES)?;
        return Ok(parse_elst(&elst, movie_timescale, media_timescale));
    }
    Ok(None)
}

/// The first edit that plays media: its `media_time` is the start to drop, its
/// `segment_duration` (in the movie timescale) the length to keep. An empty edit (a pause
/// before the media, `media_time` -1) is skipped; the track plays without that pause.
fn parse_elst(body: &[u8], movie_timescale: u32, media_timescale: u32) -> Option<EditTrim> {
    if movie_timescale == 0 || media_timescale == 0 {
        return None;
    }
    let version = *body.first()?;
    let count = u32::from_be_bytes(body.get(4..8)?.try_into().ok()?) as usize;
    let entry_len = if version == 1 { 20 } else { 12 };
    for entry in body.get(8..)?.chunks_exact(entry_len).take(count) {
        let (segment_duration, media_time) = if version == 1 {
            (
                u64::from_be_bytes(entry[0..8].try_into().ok()?),
                i64::from_be_bytes(entry[8..16].try_into().ok()?),
            )
        } else {
            (
                u64::from(u32::from_be_bytes(entry[0..4].try_into().ok()?)),
                i64::from(i32::from_be_bytes(entry[4..8].try_into().ok()?)),
            )
        };
        if media_time < 0 {
            continue;
        }
        let keep = (segment_duration > 0).then(|| {
            rescale(
                segment_duration,
                u64::from(media_timescale),
                u64::from(movie_timescale),
            )
        });
        return Some(EditTrim {
            skip: media_time as u64,
            keep,
            timescale: media_timescale,
        });
    }
    None
}

/// Whether an AAC AudioSpecificConfig signals SBR (HE-AAC): object type 5 or 29, or AAC-LC
/// followed by the `0x2B7` extension with object type 5 and the SBR flag set.
pub(super) fn asc_signals_sbr(asc: &[u8]) -> bool {
    signals_sbr(&mut BitReaderLtr::new(asc)).unwrap_or(false)
}

fn signals_sbr(bits: &mut BitReaderLtr<'_>) -> io::Result<bool> {
    let object_type = read_object_type(bits)?;
    if object_type == AOT_SBR || object_type == AOT_PS {
        return Ok(true);
    }
    // Symphonia decodes AAC-LC only; it refuses the other object types itself.
    if object_type != AOT_AAC_LC {
        return Ok(false);
    }
    if bits.read_bits_leq32(4)? == 0xF {
        bits.read_bits_leq32(24)?; // an explicit sample rate
    }
    if bits.read_bits_leq32(4)? == 0 {
        // A program config element follows; it is not parsed.
        return Ok(false);
    }
    // GASpecificConfig of AAC-LC: frameLengthFlag, dependsOnCoreCoder (+ a 14-bit delay),
    // extensionFlag (+ extensionFlag3).
    bits.read_bit()?;
    if bits.read_bit()? == 1 {
        bits.read_bits_leq32(14)?;
    }
    if bits.read_bit()? == 1 {
        bits.read_bit()?;
    }
    if bits.bits_left() < 16 || bits.read_bits_leq32(11)? != SYNC_EXTENSION {
        return Ok(false);
    }
    Ok(read_object_type(bits)? == AOT_SBR && bits.read_bit()? == 1)
}

fn read_object_type(bits: &mut BitReaderLtr<'_>) -> io::Result<u32> {
    match bits.read_bits_leq32(5)? {
        31 => Ok(32 + bits.read_bits_leq32(6)?),
        object_type => Ok(object_type),
    }
}

/// `units` converted from `from` to `to` units per second, rounded to the nearest.
fn rescale(units: u64, to: u64, from: u64) -> u64 {
    let scaled = (u128::from(units) * u128::from(to) + u128::from(from) / 2) / u128::from(from);
    u64::try_from(scaled).unwrap_or(u64::MAX)
}

struct Mp4Box {
    kind: [u8; 4],
    body: Range<u64>,
}

/// The next box in `within`; `within` then starts after it.
fn next_box<R: Read + Seek + ?Sized>(
    file: &mut R,
    within: &mut Range<u64>,
) -> io::Result<Option<Mp4Box>> {
    let left = within.end.saturating_sub(within.start);
    if left < 8 {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(within.start))?;
    let mut head = [0u8; 8];
    file.read_exact(&mut head)?;
    let kind = [head[4], head[5], head[6], head[7]];
    let (size, head_len) = match u32::from_be_bytes([head[0], head[1], head[2], head[3]]) {
        // A 64-bit size follows the type.
        1 => {
            let mut large = [0u8; 8];
            file.read_exact(&mut large)?;
            (u64::from_be_bytes(large), 16)
        }
        // The box runs to the end of its parent.
        0 => (left, 8),
        size => (u64::from(size), 8),
    };
    if size < head_len || size > left {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "mp4: a box overruns its parent",
        ));
    }
    let body = within.start + head_len..within.start + size;
    within.start += size;
    Ok(Some(Mp4Box { kind, body }))
}

fn find_box<R: Read + Seek + ?Sized>(
    file: &mut R,
    mut within: Range<u64>,
    kind: &[u8; 4],
) -> io::Result<Option<Range<u64>>> {
    while let Some(found) = next_box(file, &mut within)? {
        if &found.kind == kind {
            return Ok(Some(found.body));
        }
    }
    Ok(None)
}

/// The first `max` bytes of a box body.
fn read_body<R: Read + Seek + ?Sized>(
    file: &mut R,
    body: &Range<u64>,
    max: u64,
) -> io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(body.start))?;
    let len = (body.end - body.start).min(max);
    let mut bytes = Vec::with_capacity(len as usize);
    file.take(len).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// The `u32` after the version, flags and the creation and modification times: the timescale
/// of `mvhd` and `mdhd`, the track id of `tkhd`.
fn u32_after_times(body: &[u8]) -> Option<u32> {
    let at = if *body.first()? == 1 { 20 } else { 12 };
    Some(u32::from_be_bytes(body.get(at..at + 4)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::fixture;

    #[test]
    fn the_edit_list_gives_the_aac_priming_and_the_length() {
        let mut file = std::fs::File::open(fixture("click_left.m4a")).unwrap();
        let trim = edit_trim(&mut file, 1).unwrap();
        assert_eq!(
            trim,
            EditTrim {
                skip: 1024,
                keep: Some(44_100),
                timescale: 44_100
            }
        );
        assert_eq!(trim.at_rate(22_050), (512, Some(22_050)));
        assert_eq!(edit_trim(&mut file, 2), None, "no such track");
    }

    #[test]
    fn the_sbr_flag_is_read_from_every_explicit_form() {
        // AAC-LC 44.1 kHz stereo, as ffmpeg writes it: the extension with the SBR flag 0.
        assert!(!asc_signals_sbr(&[0x12, 0x10, 0x56, 0xe5, 0x00]));
        // AAC-LC without an extension.
        assert!(!asc_signals_sbr(&[0x12, 0x10]));
        // AOT 5 (HE-AAC) and AOT 29 (HE-AAC v2).
        assert!(asc_signals_sbr(&[0x2b, 0x92, 0x08, 0x00, 0x00]));
        assert!(asc_signals_sbr(&[0xeb, 0x8a, 0x08, 0x00]));
        // AAC-LC 22.05 kHz with the extension: AOT 5, SBR flag 1, 44.1 kHz.
        assert!(asc_signals_sbr(&[0x13, 0x90, 0x56, 0xe5, 0xa0]));
        assert!(!asc_signals_sbr(&[]));
    }
}
