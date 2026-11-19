//! The decoded-track file: interleaved stereo `i16` little-endian at 48 kHz, no header.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use super::PcmSink;

const RATE: f64 = 48_000.0;
const BYTES_PER_FRAME: u64 = 4;
/// Frames per read from the file.
const READ_FRAMES: usize = 1024;
const WRITE_BUFFER_BYTES: usize = 256 * 1024;

/// A finished decoded-track file, read at any frame position. Reads do not move a shared
/// cursor, so one `PcmFile` serves several threads.
#[derive(Debug)]
pub struct PcmFile {
    file: File,
    frames: u64,
}

impl PcmFile {
    pub fn open(path: &Path) -> io::Result<PcmFile> {
        let file = File::open(path)?;
        let frames = file.metadata()?.len() / BYTES_PER_FRAME;
        Ok(PcmFile { file, frames })
    }

    /// The number of stereo frames in the file.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    pub fn duration_s(&self) -> f64 {
        self.frames as f64 / RATE
    }

    /// Fills `out` with the interleaved stereo frames from `first_frame` on, as `f32`. Frames
    /// past the end of the file are zeros. Returns the number of frames that came from the file.
    pub fn read_frames(&self, first_frame: u64, out: &mut [f32]) -> io::Result<usize> {
        let wanted = out.len() / 2;
        let available = self.frames.saturating_sub(first_frame);
        let count = (wanted as u64).min(available) as usize;
        let mut bytes = [0u8; READ_FRAMES * BYTES_PER_FRAME as usize];
        for (index, chunk) in out[..2 * count].chunks_mut(2 * READ_FRAMES).enumerate() {
            let frame = first_frame + (index * READ_FRAMES) as u64;
            let bytes = &mut bytes[..2 * chunk.len()];
            self.file.read_exact_at(bytes, frame * BYTES_PER_FRAME)?;
            for (sample, pair) in chunk.iter_mut().zip(bytes.as_chunks::<2>().0) {
                *sample = f32::from(i16::from_le_bytes(*pair)) / 32768.0;
            }
        }
        out[2 * count..].fill(0.0);
        Ok(count)
    }
}

/// Writes a decoded track to `<dest>.part` and renames it to `<dest>` in [`finish`]. Dropped
/// before that (a failed or aborted decode), it removes the part file.
///
/// [`finish`]: PcmFileWriter::finish
pub struct PcmFileWriter {
    file: BufWriter<File>,
    part: PathBuf,
    dest: PathBuf,
    finished: bool,
    /// The bytes of one write, kept for the next one.
    bytes: Vec<u8>,
}

impl PcmFileWriter {
    pub fn create(dest: &Path) -> io::Result<PcmFileWriter> {
        let mut part = OsString::from(dest.as_os_str());
        part.push(".part");
        let part = PathBuf::from(part);
        let file = File::create(&part)?;
        Ok(PcmFileWriter {
            file: BufWriter::with_capacity(WRITE_BUFFER_BYTES, file),
            part,
            dest: dest.to_path_buf(),
            finished: false,
            bytes: Vec::new(),
        })
    }

    /// Writes the rest out and gives the file its final name.
    pub fn finish(mut self) -> io::Result<()> {
        self.file.flush()?;
        std::fs::rename(&self.part, &self.dest)?;
        self.finished = true;
        Ok(())
    }
}

impl PcmSink for PcmFileWriter {
    fn write(&mut self, stereo_48k: &[f32]) -> io::Result<()> {
        self.bytes.clear();
        for &sample in stereo_48k {
            self.bytes.extend_from_slice(&to_i16(sample).to_le_bytes());
        }
        self.file.write_all(&self.bytes)
    }
}

impl Drop for PcmFileWriter {
    fn drop(&mut self) {
        if !self.finished {
            let _ = std::fs::remove_file(&self.part);
        }
    }
}

/// Full scale is 32768, as in [`PcmFile::read_frames`]; louder samples are clamped.
fn to_i16(sample: f32) -> i16 {
    (sample * 32768.0).round().clamp(-32768.0, 32767.0) as i16
}
