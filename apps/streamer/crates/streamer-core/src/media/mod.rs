//! Audio files: the decoder that writes a track as a 48 kHz stereo file, and that file.

mod decode;
mod mp3;
mod mp4;
mod pcm_file;

use std::error::Error;

pub use decode::{MAX_TRACK_SECONDS, PcmSink, decode_bytes, decode_to, decode_track};
pub use pcm_file::{PcmFile, PcmFileWriter};

/// The track cannot be played. The text is what the queue shows.
#[derive(Debug)]
pub enum MediaError {
    /// The track cannot be read or decoded. The cause stays in `source()` for logs.
    Unplayable(Box<dyn Error + Send + Sync>),
    /// The track is longer than [`MAX_TRACK_SECONDS`].
    TooLong,
}

impl MediaError {
    fn new(cause: impl Into<Box<dyn Error + Send + Sync>>) -> Self {
        Self::Unplayable(cause.into())
    }
}

impl std::fmt::Display for MediaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unplayable(_) => f.write_str("Cannot play this track"),
            Self::TooLong => f.write_str("Track is longer than 3 hours"),
        }
    }
}

impl Error for MediaError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Unplayable(cause) => Some(cause.as_ref()),
            Self::TooLong => None,
        }
    }
}

#[cfg(test)]
fn fixture(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/media")
        .join(name)
}
