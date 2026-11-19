//! Decodes a track to 48 kHz stereo in small parts: Symphonia decodes (libopus decodes Opus),
//! the channels are mixed down to stereo, and rubato resamples to 48 kHz. Each part goes to a
//! [`PcmSink`] at once, so memory does not grow with the length of the track.

use std::fs::File;
use std::io::{self, Cursor};
use std::ops::RangeInclusive;
use std::path::Path;
use std::sync::OnceLock;

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Indexing, Resampler};
use symphonia::core::codecs::audio::well_known::{CODEC_ID_AAC, CODEC_ID_OPUS};
use symphonia::core::codecs::audio::{AudioCodecParameters, AudioDecoderOptions};
use symphonia::core::codecs::registry::CodecRegistry;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::well_known::{
    FORMAT_ID_ADTS, FORMAT_ID_ISOMP4, FORMAT_ID_MP1, FORMAT_ID_MP2, FORMAT_ID_MP3,
};
use symphonia::core::formats::{FormatId, FormatOptions, Track, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia_adapter_libopus::OpusDecoder;

use super::mp3;
use super::mp4::{self, EditTrim};
use super::{MediaError, PcmFileWriter};

/// A longer track is an error (it would take about 2 GB on disk).
pub const MAX_TRACK_SECONDS: f64 = 3.0 * 3600.0;
const RATE: u32 = 48_000;
/// The source sample rates that play: the `AudioBuffer` range of Web Audio. The resampler for a
/// rate far outside would need gigabytes.
const SOURCE_RATES: RangeInclusive<u32> = 3_000..=768_000;
/// Input frames per resampler call.
const RESAMPLE_CHUNK: usize = 1024;
/// A track that decodes to fewer frames than this share of the frame count in its header is
/// cut short (a truncated download decodes without an error).
const MIN_DECODED_SHARE: f64 = 0.98;

/// Receives a decoded track: interleaved stereo at 48 kHz, in order, in small parts.
pub trait PcmSink {
    fn write(&mut self, stereo_48k: &[f32]) -> io::Result<()>;

    /// True when the decode must stop. The decoder asks before every packet, so a file whose
    /// packets do not decode (and so write nothing) also stops at once.
    fn cancelled(&self) -> bool {
        false
    }
}

impl PcmSink for Vec<f32> {
    fn write(&mut self, stereo_48k: &[f32]) -> io::Result<()> {
        self.extend_from_slice(stereo_48k);
        Ok(())
    }
}

/// Decodes the audio file at `path` into `sink`. Blocking. Returns the number of 48 kHz
/// frames written.
pub fn decode_to(path: &Path, sink: &mut dyn PcmSink) -> Result<u64, MediaError> {
    let open = || -> io::Result<Box<dyn MediaSource>> { Ok(Box::new(File::open(path)?)) };
    decode_source(&open, path.extension().and_then(|e| e.to_str()), sink)
}

/// Decodes the audio file at `path` into the decoded-track file `dest` (see
/// [`PcmFile`](super::PcmFile)). Blocking. On failure no file is left at `dest`.
pub fn decode_track(path: &Path, dest: &Path) -> Result<u64, MediaError> {
    let mut writer = PcmFileWriter::create(dest).map_err(MediaError::new)?;
    let frames = decode_to(path, &mut writer)?;
    writer.finish().map_err(MediaError::new)?;
    Ok(frames)
}

/// Decodes an audio file held in memory (a bundled sound). `ext` is its file extension.
pub fn decode_bytes(bytes: &'static [u8], ext: &str) -> Result<Vec<f32>, MediaError> {
    let open = || -> io::Result<Box<dyn MediaSource>> { Ok(Box::new(Cursor::new(bytes))) };
    let mut pcm = Vec::new();
    decode_source(&open, Some(ext), &mut pcm)?;
    Ok(pcm)
}

/// Symphonia's codecs and libopus for Opus.
fn codec_registry() -> &'static CodecRegistry {
    static REGISTRY: OnceLock<CodecRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut registry = CodecRegistry::new();
        symphonia::default::register_enabled_codecs(&mut registry);
        registry.register_audio_decoder::<OpusDecoder>();
        registry
    })
}

/// `open` gives the file from its start; it is called again to read an MP4 edit list or the
/// first MP3 frame.
fn decode_source(
    open: &dyn Fn() -> io::Result<Box<dyn MediaSource>>,
    extension: Option<&str>,
    sink: &mut dyn PcmSink,
) -> Result<u64, MediaError> {
    let stream = MediaSourceStream::new(open().map_err(MediaError::new)?, Default::default());
    let mut hint = Hint::new();
    if let Some(extension) = extension {
        hint.with_extension(extension);
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
    let params = track
        .codec_params
        .as_ref()
        .and_then(|params| params.audio())
        .ok_or_else(|| MediaError::new("no audio codec parameters"))?;
    let header_frames = exact_num_frames(format.format_info().format, &track, open);
    if let (Some(frames), Some(rate)) = (header_frames, params.sample_rate) {
        check_length(frames_at_48k(frames, rate))?;
    }
    check_codec(params)?;
    // Symphonia does not drop the encoder delay of AAC in MP4; the edit list gives it.
    let trim = if format.format_info().format == FORMAT_ID_ISOMP4 && params.codec == CODEC_ID_AAC {
        open()
            .ok()
            .and_then(|mut file| mp4::edit_trim(&mut *file, track.id))
    } else {
        None
    };
    let mut decoder = codec_registry()
        .make_audio_decoder(params, &AudioDecoderOptions::default())
        .map_err(MediaError::new)?;

    let mut output: Option<Output> = None;
    let mut samples: Vec<f32> = Vec::new();
    let (mut packets, mut skipped, mut decoded) = (0u64, 0u64, 0u64);
    loop {
        if sink.cancelled() {
            return Err(MediaError::new(io::Error::new(
                io::ErrorKind::Interrupted,
                "the decode was cancelled",
            )));
        }
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            // Some demuxers report the end of the file as an I/O error.
            Err(SymphoniaError::IoError(e)) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            // A chained stream follows (Ogg): the track ends here.
            Err(SymphoniaError::ResetRequired) => break,
            Err(e) => return Err(MediaError::new(e)),
        };
        if packet.track_id != track.id {
            continue;
        }
        packets += 1;
        let buffer = match decoder.decode(&packet) {
            Ok(buffer) => buffer,
            Err(SymphoniaError::DecodeError(_) | SymphoniaError::IoError(_)) => {
                skipped += 1;
                continue;
            }
            Err(e) => return Err(MediaError::new(e)),
        };
        let (rate, channels) = (buffer.spec().rate(), buffer.spec().channels().count());
        let output = match &mut output {
            Some(output) if (output.rate, output.channels) == (rate, channels) => output,
            Some(_) => {
                return Err(MediaError::new(
                    "the sample rate or the channel count changes in the track",
                ));
            }
            None => {
                // HE-AAC without the signaling that `check_codec` sees: the decoder drops the
                // high band and runs at half the rate.
                if params.codec == CODEC_ID_AAC
                    && params.sample_rate.is_some_and(|container| rate < container)
                {
                    return Err(MediaError::new("HE-AAC is not supported"));
                }
                output.insert(Output::new(rate, channels, trim)?)
            }
        };
        buffer.copy_to_vec_interleaved(&mut samples);
        decoded += buffer.frames() as u64;
        output.push(&samples, sink)?;
    }

    if skipped * 10 > packets {
        return Err(MediaError::new(format!(
            "{skipped} of {packets} packets cannot be decoded"
        )));
    }
    if let Some(expected) = header_frames
        && (decoded as f64) < MIN_DECODED_SHARE * expected as f64
    {
        return Err(MediaError::new(format!(
            "the file ends early: {decoded} of {expected} frames"
        )));
    }
    let frames = match output {
        Some(output) => output.finish(sink)?,
        None => 0,
    };
    if frames == 0 {
        return Err(MediaError::new("the track has no audio"));
    }
    Ok(frames)
}

/// The frame count in the header of `track`, when the reader read it rather than estimated it.
/// The ADTS reader always estimates it, and the MP3 reader does so when the first frame has no
/// Xing, Info or VBRI tag: from the file size and the sizes of the first frames. A quiet start
/// (small frames) makes such an estimate far too long. It comes from the real file size, so it
/// cannot show a cut file either.
fn exact_num_frames(
    format: FormatId,
    track: &Track,
    open: &dyn Fn() -> io::Result<Box<dyn MediaSource>>,
) -> Option<u64> {
    let frames = track.num_frames?;
    match format {
        FORMAT_ID_ADTS => None,
        FORMAT_ID_MP1 | FORMAT_ID_MP2 | FORMAT_ID_MP3 => {
            let mut file = open().ok()?;
            mp3::has_length_tag(&mut *file).then_some(frames)
        }
        _ => Some(frames),
    }
}

/// Rejects what the decoders would play wrongly or not at all.
fn check_codec(params: &AudioCodecParameters) -> Result<(), MediaError> {
    if params.codec == CODEC_ID_OPUS && params.channels.as_ref().is_some_and(|c| c.count() > 2) {
        return Err(MediaError::new(
            "Opus with more than two channels is not supported",
        ));
    }
    if params.codec == CODEC_ID_AAC
        && params
            .extra_data
            .as_deref()
            .is_some_and(mp4::asc_signals_sbr)
    {
        return Err(MediaError::new("HE-AAC is not supported"));
    }
    Ok(())
}

fn check_rate(rate: u32) -> Result<(), MediaError> {
    if !SOURCE_RATES.contains(&rate) {
        return Err(MediaError::new(format!(
            "the sample rate {rate} Hz is outside 3000 to 768000 Hz"
        )));
    }
    Ok(())
}

fn check_length(frames_48k: u64) -> Result<(), MediaError> {
    if frames_48k as f64 / f64::from(RATE) > MAX_TRACK_SECONDS {
        return Err(MediaError::TooLong);
    }
    Ok(())
}

/// `frames` at `rate` in 48 kHz frames, rounded up.
fn frames_at_48k(frames: u64, rate: u32) -> u64 {
    let scaled = (u128::from(frames) * u128::from(RATE)).div_ceil(u128::from(rate.max(1)));
    u64::try_from(scaled).unwrap_or(u64::MAX)
}

/// Turns decoded buffers into 48 kHz stereo for the sink: the edit-list trim, the down-mix
/// and the resampler.
struct Output {
    rate: u32,
    channels: usize,
    /// Decoded frames still to drop at the start.
    skip: u64,
    /// Decoded frames still to keep; `None` keeps all.
    keep: Option<u64>,
    stereo: Vec<f32>,
    /// `None` at 48 kHz.
    resampler: Option<Resampler48>,
    written: Written,
}

impl Output {
    fn new(rate: u32, channels: usize, trim: Option<EditTrim>) -> Result<Output, MediaError> {
        check_rate(rate)?;
        if channels == 0 {
            return Err(MediaError::new("the track has no channels"));
        }
        let (skip, keep) = trim.map_or((0, None), |trim| trim.at_rate(rate));
        let resampler = if rate == RATE {
            None
        } else {
            Some(Resampler48::new(rate)?)
        };
        Ok(Output {
            rate,
            channels,
            skip,
            keep,
            stereo: Vec::new(),
            resampler,
            written: Written::default(),
        })
    }

    /// `samples` are interleaved frames of `self.channels` channels.
    fn push(&mut self, samples: &[f32], sink: &mut dyn PcmSink) -> Result<(), MediaError> {
        let frames = (samples.len() / self.channels) as u64;
        let skip = self.skip.min(frames);
        self.skip -= skip;
        let mut take = frames - skip;
        if let Some(keep) = &mut self.keep {
            take = take.min(*keep);
            *keep -= take;
        }
        let start = skip as usize * self.channels;
        let end = start + take as usize * self.channels;
        downmix(&samples[start..end], self.channels, &mut self.stereo);
        if self.stereo.is_empty() {
            return Ok(());
        }
        let written = &mut self.written;
        match &mut self.resampler {
            Some(resampler) => {
                resampler.push(&self.stereo, &mut |stereo| written.write(stereo, sink))
            }
            None => written.write(&self.stereo, sink),
        }
    }

    /// Writes the end of the resampled audio. Returns the number of frames written.
    fn finish(mut self, sink: &mut dyn PcmSink) -> Result<u64, MediaError> {
        let written = &mut self.written;
        if let Some(resampler) = &mut self.resampler {
            resampler.finish(&mut |stereo| written.write(stereo, sink))?;
        }
        Ok(self.written.frames)
    }
}

/// Counts the 48 kHz frames on their way to the sink and stops a track that runs too long.
#[derive(Default)]
struct Written {
    frames: u64,
}

impl Written {
    fn write(&mut self, stereo: &[f32], sink: &mut dyn PcmSink) -> Result<(), MediaError> {
        self.frames += (stereo.len() / 2) as u64;
        check_length(self.frames)?;
        sink.write(stereo).map_err(MediaError::new)
    }
}

/// Mixes interleaved frames of `channels` channels down to stereo by the Web Audio "speakers"
/// rules: mono plays on both sides; quad: L = ½(L + SL), R = ½(R + SR); 5.1:
/// L = L + √½(C + SL), R = R + √½(C + SR), without the LFE; any other count keeps the first
/// two channels.
fn downmix(samples: &[f32], channels: usize, stereo: &mut Vec<f32>) {
    use std::f32::consts::FRAC_1_SQRT_2;
    stereo.clear();
    match channels {
        1 => stereo.extend(samples.iter().flat_map(|&s| [s, s])),
        2 => stereo.extend_from_slice(samples),
        4 => stereo.extend(
            samples
                .as_chunks::<4>()
                .0
                .iter()
                .flat_map(|f| [0.5 * (f[0] + f[2]), 0.5 * (f[1] + f[3])]),
        ),
        6 => stereo.extend(samples.as_chunks::<6>().0.iter().flat_map(|f| {
            [
                f[0] + FRAC_1_SQRT_2 * (f[2] + f[4]),
                f[1] + FRAC_1_SQRT_2 * (f[2] + f[5]),
            ]
        })),
        _ => stereo.extend(samples.chunks_exact(channels).flat_map(|f| [f[0], f[1]])),
    }
}

/// rubato's FFT resampler from `rate` to 48 kHz, fed in chunks of [`RESAMPLE_CHUNK`] stereo
/// frames. The output equals `process_all` over the whole track: the resampler delay is
/// dropped at the start, and at the end zeros go in until exactly ⌈n · 48000 / rate⌉ frames
/// are out.
struct Resampler48 {
    fft: Fft<f32>,
    rate: u32,
    /// Interleaved stereo input, `staged` frames of it filled.
    input: Vec<f32>,
    staged: usize,
    /// Interleaved stereo output of one call.
    output: Vec<f32>,
    /// Output frames still to drop (the resampler delay).
    delay: usize,
    frames_in: u64,
    frames_out: u64,
}

type Emit<'a> = dyn FnMut(&[f32]) -> Result<(), MediaError> + 'a;

impl Resampler48 {
    fn new(rate: u32) -> Result<Resampler48, MediaError> {
        let fft = Fft::<f32>::new(
            rate as usize,
            RATE as usize,
            RESAMPLE_CHUNK,
            2,
            FixedSync::Input,
        )
        .map_err(MediaError::new)?;
        Ok(Resampler48 {
            rate,
            input: vec![0.0; 2 * RESAMPLE_CHUNK],
            staged: 0,
            output: vec![0.0; 2 * fft.output_frames_max()],
            delay: fft.output_delay(),
            frames_in: 0,
            frames_out: 0,
            fft,
        })
    }

    fn push(&mut self, stereo: &[f32], emit: &mut Emit<'_>) -> Result<(), MediaError> {
        self.frames_in += (stereo.len() / 2) as u64;
        let mut rest = stereo;
        while !rest.is_empty() {
            let take = (2 * (RESAMPLE_CHUNK - self.staged)).min(rest.len());
            self.input[2 * self.staged..2 * self.staged + take].copy_from_slice(&rest[..take]);
            self.staged += take / 2;
            rest = &rest[take..];
            if self.staged == RESAMPLE_CHUNK {
                self.process(None, u64::MAX, emit)?;
                self.staged = 0;
            }
        }
        Ok(())
    }

    fn finish(&mut self, emit: &mut Emit<'_>) -> Result<(), MediaError> {
        let total = frames_at_48k(self.frames_in, self.rate);
        if self.staged > 0 {
            self.process(Some(self.staged), total, emit)?;
            self.staged = 0;
        }
        while self.frames_out < total {
            self.process(Some(0), total, emit)?;
        }
        Ok(())
    }

    /// One resampler call. `partial` is the number of staged frames when fewer than a chunk
    /// (the rest is silence); at most `total` frames go out in all.
    fn process(
        &mut self,
        partial: Option<usize>,
        total: u64,
        emit: &mut Emit<'_>,
    ) -> Result<(), MediaError> {
        let produced = {
            let input = InterleavedSlice::new(&self.input[..], 2, RESAMPLE_CHUNK)
                .map_err(MediaError::new)?;
            let frames = self.output.len() / 2;
            let mut output = InterleavedSlice::new_mut(&mut self.output[..], 2, frames)
                .map_err(MediaError::new)?;
            let indexing = partial.map(|frames| Indexing::new().partial_len(frames));
            let (_, produced) = self
                .fft
                .process_into_buffer(&input, &mut output, indexing.as_ref())
                .map_err(MediaError::new)?;
            produced
        };
        let dropped = self.delay.min(produced);
        self.delay -= dropped;
        let count = total
            .saturating_sub(self.frames_out)
            .min((produced - dropped) as u64) as usize;
        self.frames_out += count as u64;
        if count > 0 {
            emit(&self.output[2 * dropped..2 * (dropped + count)])?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::*;
    use crate::media::{PcmFile, fixture};
    use crate::test_util::TempDir;

    const CLICKS: [&str; 9] = [
        "click_left.wav",
        "click_left.mp3",
        "click_left.m4a",
        "click_left_alac.m4a",
        "click_left.flac",
        "click_left.ogg",
        "click_left.opus",
        "click_left.webm",
        // Resampled down to 48 kHz.
        "click_left_96k.flac",
    ];
    /// Frame 24 000 at 48 kHz: where the click of every fixture starts.
    const CLICK_FRAME: usize = 24_000;

    fn decode(name: &str) -> Vec<f32> {
        let mut pcm = Vec::new();
        let frames = decode_to(&fixture(name), &mut pcm)
            .unwrap_or_else(|e| panic!("{name}: {e} {:?}", e.source()));
        assert_eq!(pcm.len() as u64, 2 * frames, "{name}");
        pcm
    }

    fn channel(pcm: &[f32], index: usize) -> Vec<f32> {
        pcm.iter().skip(index).step_by(2).copied().collect()
    }

    fn peak_index(samples: &[f32]) -> usize {
        let mut peak = 0;
        for (i, s) in samples.iter().enumerate() {
            if s.abs() > samples[peak].abs() {
                peak = i;
            }
        }
        peak
    }

    fn rms(samples: &[f32]) -> f64 {
        let sum: f64 = samples.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
        (sum / samples.len() as f64).sqrt()
    }

    #[test]
    fn every_format_decodes_to_one_second_at_48k() {
        for name in CLICKS.iter().chain(&["mono_22k.wav"]) {
            let frames = decode(name).len() / 2;
            // WebM Opus keeps its end padding (at most one packet).
            let expected = if name.ends_with(".webm") {
                48_000..=48_960
            } else {
                47_999..=48_001
            };
            assert!(expected.contains(&frames), "{name}: {frames} frames");
        }
    }

    #[test]
    fn the_click_lands_at_half_a_second() {
        for name in CLICKS {
            let pcm = decode(name);
            let (left, right) = (channel(&pcm, 0), channel(&pcm, 1));
            let peak = peak_index(&left);
            assert!(peak.abs_diff(CLICK_FRAME) <= 48, "{name}: peak at {peak}");
            assert!(
                rms(&right) < 0.01 * rms(&left),
                "{name}: right {} left {}",
                rms(&right),
                rms(&left)
            );
        }
    }

    #[test]
    fn the_file_matches_the_vec_sink() {
        let dir = TempDir::new("decode");
        let dest = dir.0.join("click.pcm");
        let frames = decode_track(&fixture("click_left.flac"), &dest).unwrap();
        let pcm = decode("click_left.flac");
        assert_eq!(2 * frames as usize, pcm.len());

        let file = PcmFile::open(&dest).unwrap();
        assert_eq!(file.frames(), frames);
        let mut read = vec![0.0; pcm.len()];
        assert_eq!(file.read_frames(0, &mut read).unwrap(), frames as usize);
        for (i, (from_file, from_vec)) in read.iter().zip(&pcm).enumerate() {
            // The file holds 16-bit samples: a value past full scale is clamped.
            let expected = from_vec.clamp(-1.0, 1.0);
            assert!(
                (from_file - expected).abs() <= 1.0 / 32768.0,
                "sample {i}: {from_file} vs {from_vec}"
            );
        }
        assert_eq!(dir.files(), ["click.pcm"], "no part file is left");

        let failed = dir.0.join("broken.pcm");
        decode_track(&fixture("broken.mp3"), &failed).expect_err("must fail");
        assert_eq!(dir.files(), ["click.pcm"], "a failed decode leaves no file");
    }

    #[test]
    fn pcm_file_reads_at_any_position() {
        let dir = TempDir::new("decode");
        let dest = dir.0.join("click.pcm");
        decode_track(&fixture("click_left.wav"), &dest).unwrap();
        let file = PcmFile::open(&dest).unwrap();
        assert_eq!(file.duration_s(), file.frames() as f64 / 48_000.0);

        let mut block = [0.0f32; 2 * 960];
        let first = CLICK_FRAME as u64 - 10;
        assert_eq!(file.read_frames(first, &mut block).unwrap(), 960);
        let onset = channel(&block, 0).iter().position(|s| s.abs() > 0.5);
        assert_eq!(onset, Some(10), "the click starts 10 frames in");
        let pcm = decode("click_left.wav");
        let expected = &pcm[2 * first as usize..2 * first as usize + block.len()];
        for (from_file, from_vec) in block.iter().zip(expected) {
            assert!((from_file - from_vec).abs() <= 1.0 / 32768.0);
        }

        let mut tail = [1.0f32; 2 * 960];
        assert_eq!(
            file.read_frames(file.frames() - 100, &mut tail).unwrap(),
            100
        );
        assert!(tail[200..].iter().all(|&s| s == 0.0), "zeros past the end");
        let mut past = [1.0f32; 2 * 960];
        assert_eq!(file.read_frames(file.frames() + 5, &mut past).unwrap(), 0);
        assert!(past.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn mono_plays_on_both_channels() {
        let pcm = decode("mono_22k.wav");
        assert!(pcm.chunks(2).all(|frame| frame[0] == frame[1]));
        let left = channel(&pcm, 0);
        let peak = peak_index(&left);
        assert!(peak.abs_diff(CLICK_FRAME) <= 48, "peak at {peak}");
        assert!(left[peak].abs() > 0.5, "{}", left[peak]);
    }

    #[test]
    fn five_one_mixes_the_centre_into_both_channels() {
        let pcm = decode("six_ch.flac");
        // The click is in the centre channel only and starts at 16-bit full scale.
        let centre_peak = 32767.0 / 32768.0;
        for index in 0..2 {
            let samples = channel(&pcm, index);
            let peak = peak_index(&samples);
            assert_eq!(peak, CLICK_FRAME, "channel {index}");
            let expected = std::f32::consts::FRAC_1_SQRT_2 * centre_peak;
            assert!(
                (samples[peak] - expected).abs() < 1e-3,
                "channel {index}: {} vs {expected}",
                samples[peak]
            );
        }
    }

    #[test]
    fn he_aac_and_broken_files_are_errors() {
        for name in ["he_explicit.m4a", "truncated.mp3", "broken.mp3"] {
            let mut pcm = Vec::new();
            let err = decode_to(&fixture(name), &mut pcm).expect_err(name);
            assert_eq!(err.to_string(), "Cannot play this track", "{name}");
            assert!(err.source().is_some(), "{name}");
        }
    }

    #[test]
    fn chunked_resampling_equals_process_all() {
        let frames = 5_000;
        let input: Vec<f32> = (0..2 * frames)
            .map(|i| ((i * 7919) % 1000) as f32 / 1000.0 - 0.5)
            .collect();
        let mut whole =
            Fft::<f32>::new(44_100, 48_000, RESAMPLE_CHUNK, 2, FixedSync::Input).unwrap();
        let slice = InterleavedSlice::new(&input[..], 2, frames).unwrap();
        let expected = whole.process_all(&slice, frames, None).unwrap().take_data();

        let mut out = Vec::new();
        {
            let mut emit = |stereo: &[f32]| -> Result<(), MediaError> {
                out.extend_from_slice(stereo);
                Ok(())
            };
            let mut resampler = Resampler48::new(44_100).unwrap();
            for part in input.chunks(2 * 333) {
                resampler.push(part, &mut emit).unwrap();
            }
            resampler.finish(&mut emit).unwrap();
        }
        // ⌈5000 · 48000 / 44100⌉ frames.
        assert_eq!(out.len(), 2 * 5_443);
        assert_eq!(out, expected);
    }

    #[test]
    fn a_length_estimated_from_the_file_size_is_not_checked() {
        // ADTS and an MP3 without a Xing tag: the reader estimates the frame count from the file
        // size, and the silent start makes the estimate too long (8 % and 2.6 times).
        for (name, seconds) in [("quiet_start.aac", 7.0), ("quiet_start_noxing.mp3", 3.0)] {
            let frames = decode(name).len() / 2;
            // At most 0.1 s of encoder delay and padding on top.
            let expected = (seconds * 48_000.0) as usize..=((seconds + 0.1) * 48_000.0) as usize;
            assert!(expected.contains(&frames), "{name}: {frames} frames");
        }
    }

    #[test]
    fn a_sample_rate_outside_3k_to_768k_is_an_error() {
        for rate in [1, 2_999, 768_001, u32::MAX] {
            assert!(check_rate(rate).is_err(), "{rate} Hz");
        }
        for rate in [3_000, 44_100, 768_000] {
            assert!(check_rate(rate).is_ok(), "{rate} Hz");
        }

        // A WAV file at 2 kHz: mono 16-bit, 0.5 s of silence.
        let samples = 1_000u32;
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + 2 * samples).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&2_000u32.to_le_bytes()); // sample rate
        wav.extend_from_slice(&4_000u32.to_le_bytes()); // byte rate
        wav.extend_from_slice(&2u16.to_le_bytes()); // block align
        wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(2 * samples).to_le_bytes());
        wav.resize(wav.len() + 2 * samples as usize, 0);
        let dir = TempDir::new("decode");
        let path = dir.0.join("slow.wav");
        std::fs::write(&path, wav).unwrap();
        let err = decode_to(&path, &mut Vec::new()).expect_err("2 kHz");
        assert_eq!(err.to_string(), "Cannot play this track");
    }

    #[test]
    fn a_track_over_3_hours_is_too_long() {
        let three_hours = 3 * 3600 * 48_000;
        assert!(check_length(three_hours).is_ok());
        let err = check_length(three_hours + 1).expect_err("too long");
        assert!(matches!(err, MediaError::TooLong));
        assert_eq!(err.to_string(), "Track is longer than 3 hours");
    }

    #[test]
    fn a_cancelled_decode_stops_before_its_next_packet() {
        // A sink whose writes never fail: only the check before each packet can stop it.
        struct Cancelled(usize);
        impl PcmSink for Cancelled {
            fn write(&mut self, stereo_48k: &[f32]) -> io::Result<()> {
                self.0 += stereo_48k.len();
                Ok(())
            }
            fn cancelled(&self) -> bool {
                true
            }
        }
        let mut sink = Cancelled(0);
        let err = decode_to(&fixture("click_left.flac"), &mut sink).expect_err("cancelled");
        assert_eq!(err.to_string(), "Cannot play this track");
        let cause = err.source().map(ToString::to_string);
        assert_eq!(cause.as_deref(), Some("the decode was cancelled"));
        assert_eq!(sink.0, 0, "nothing is written");
    }

    #[test]
    fn the_kick_decodes_from_bytes() {
        let kick = decode_bytes(include_bytes!("../../assets/kick.wav"), "wav").unwrap();
        // 9649 frames at 44.1 kHz (0.219 s): ⌈9649 · 48000 / 44100⌉.
        assert_eq!(kick.len() / 2, 10_503);
    }
}
