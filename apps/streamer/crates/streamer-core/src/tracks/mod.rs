//! The queue tracks: the pure planner that decides which track to load and which decoded
//! files to keep, the loader that downloads the tracks into the cache dir, decodes them and
//! acknowledges the load requests, the pure player that applies the web client's playback
//! rules, and the feeder that queues the playing track for the output.

pub(crate) mod feeder;
pub(crate) mod loader;
mod planner;
mod player;

pub use planner::{
    MAX_DECODED, MAX_DOWNLOADS, TrackAction, TrackPlanner, TrackState, TrackStates,
    decoded_file_name, track_file_name,
};
pub use player::{PlayerAction, QueuePlayer};

/// A URL field of a room message; an empty one is none.
fn non_empty(value: &serde_json::Value) -> Option<&str> {
    value.as_str().filter(|url| !url.is_empty())
}
