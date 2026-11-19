use std::error::Error;
use std::fs::File;
use std::path::Path;

use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, Track, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::{Duration, Time};

const TAIL_PROBE_SECONDS: f64 = 0.5;

/// The track cannot be read as audio. The cause stays in `source()` for logs.
#[derive(Debug)]
pub struct MediaError {
    cause: Box<dyn Error + Send + Sync>,
}

impl MediaError {
    fn new(cause: impl Into<Box<dyn Error + Send + Sync>>) -> Self {
        Self {
            cause: cause.into(),
        }
    }
}

impl std::fmt::Display for MediaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Cannot play this track")
    }
}

impl Error for MediaError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.cause.as_ref())
    }
}

/// Length of an audio file in seconds. Blocking: the caller uses `spawn_blocking`.
///
/// Reads the container header first. When the header has no length, it scans the
/// packets without decoding them.
pub fn probe_duration(path: &Path) -> Result<f64, MediaError> {
    let file = File::open(path).map_err(MediaError::new)?;
    let stream = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            stream,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(MediaError::new)?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| MediaError::new("no audio track"))?
        .clone();
    let seconds = match header_seconds(&track) {
        Some(seconds) => {
            check_tail(format.as_mut(), &track, seconds).map_err(MediaError::new)?;
            seconds
        }
        None => scan_seconds(format.as_mut(), &track).map_err(MediaError::new)?,
    };
    if seconds.is_finite() && seconds > 0.0 {
        Ok(seconds)
    } else {
        Err(MediaError::new("the track has no length"))
    }
}

/// A header can promise more audio than the file holds (a cut or damaged file).
/// Seek close to the promised end and read one packet to see that the audio is there.
fn check_tail(
    format: &mut dyn FormatReader,
    track: &Track,
    seconds: f64,
) -> Result<(), SymphoniaError> {
    let time = Time::try_from_secs_f64((seconds - TAIL_PROBE_SECONDS).max(0.0))
        .ok_or(SymphoniaError::DecodeError("the track length overflows"))?;
    format.seek(
        SeekMode::Coarse,
        SeekTo::Time {
            time,
            track_id: Some(track.id),
        },
    )?;
    match format.next_packet()? {
        Some(_) => Ok(()),
        None => Err(SymphoniaError::DecodeError(
            "the file ends before the audio does",
        )),
    }
}

fn header_seconds(track: &Track) -> Option<f64> {
    if let (Some(duration), Some(time_base)) = (track.duration, track.time_base) {
        return Some(time_base.calc_duration(duration)?.as_secs_f64());
    }
    let sample_rate = sample_rate(track)?;
    Some(track.num_frames? as f64 / f64::from(sample_rate))
}

fn sample_rate(track: &Track) -> Option<u32> {
    match track.codec_params.as_ref()? {
        symphonia::core::codecs::CodecParameters::Audio(params) => params.sample_rate,
        _ => None,
    }
}

/// Reads the packets only and takes the end of the last one. No decoding.
fn scan_seconds(format: &mut dyn FormatReader, track: &Track) -> Result<f64, SymphoniaError> {
    let mut end = track.start_ts;
    loop {
        match format.next_packet() {
            Ok(Some(packet)) => {
                if packet.track_id == track.id {
                    end = end.max(packet.pts.saturating_add(packet.dur));
                }
            }
            // Some demuxers (WebM) report the end of the file as an I/O error.
            Ok(None) => break,
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(e) => return Err(e),
        }
    }
    let ticks = end.get().saturating_sub(track.start_ts.get());
    let ticks = Duration::new(ticks.max(0) as u64);
    if let Some(time_base) = track.time_base {
        return time_base
            .calc_duration(ticks)
            .map(|t| t.as_secs_f64())
            .ok_or(SymphoniaError::DecodeError("the track length overflows"));
    }
    let sample_rate = sample_rate(track).ok_or(SymphoniaError::Unsupported("no time base"))?;
    Ok(ticks.get() as f64 / f64::from(sample_rate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TempDir;

    fn fixture(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/media")
            .join(name)
    }

    #[test]
    fn every_supported_format_reports_its_duration() {
        for name in [
            "tone.mp3",
            "tone.m4a",
            "tone.flac",
            "tone.wav",
            "tone.ogg",
            "tone-alac.m4a",
            "tone.opus",
            "tone.webm",
        ] {
            let secs = probe_duration(&fixture(name))
                .unwrap_or_else(|e| panic!("{name}: {e} {:?}", e.source()));
            assert!((secs - 2.0).abs() <= 0.06, "{name}: {secs}");
        }
    }

    #[test]
    fn a_webm_without_a_length_is_scanned_by_packets() {
        // Made with `-live 1`: the header has no duration.
        let secs = probe_duration(&fixture("tone-live.webm")).unwrap();
        assert!((secs - 2.0).abs() <= 0.06, "{secs}");
    }

    #[test]
    fn a_broken_file_is_an_error() {
        let dir = TempDir::new("media");
        let text = dir.0.join("note.mp3");
        std::fs::write(&text, "this is not audio at all").unwrap();
        let empty = dir.0.join("empty.wav");
        std::fs::write(&empty, "").unwrap();
        for path in [
            fixture("broken.mp3"),
            text,
            empty,
            dir.0.join("missing.mp3"),
        ] {
            let err = probe_duration(&path).expect_err("must fail");
            assert_eq!(err.to_string(), "Cannot play this track");
            assert!(err.source().is_some(), "{path:?}");
        }
    }
}
