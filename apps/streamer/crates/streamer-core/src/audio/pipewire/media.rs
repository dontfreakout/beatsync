//! What the capture and the local playback stream share: the format, the `pw_time` fields the
//! timing rules use, and a stream handle that tears down in a safe order.
//!
//! Both streams use `RT_PROCESS`: their `process` callback runs on PipeWire's real-time data
//! thread. It owns its state (moved in at creation) and must not allocate, lock or log. All
//! other callbacks run on the PipeWire thread and get no access to that state.

use ::pipewire as pw;
use pw::core::CoreRc;
use pw::properties::PropertiesBox;
use pw::spa;
use pw::spa::pod::Pod;
use pw::spa::utils::Direction;
use pw::stream::{Stream, StreamFlags, StreamListener, StreamRc, StreamState};

/// Sample rate of both streams; the stream adapters convert from the graph rate.
pub(super) const RATE: u32 = 48_000;
pub(super) const CHANNELS: usize = 2;
/// Bytes of one interleaved F32LE stereo frame.
pub(super) const STRIDE: usize = CHANNELS * 4;
/// The quantum both streams ask for: 20 ms. The graph may use another one (often 512 or 1024).
pub(super) const LATENCY: &str = "960/48000";

/// The `pw_time` fields of one graph cycle that the timing rules use.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Timing {
    /// CLOCK_MONOTONIC ns of the cycle (`pw_time.now`).
    pub now_ns: i64,
    /// `pw_time.rate`: one tick is `num / denom` s (usually 1/48000).
    pub rate_num: u32,
    pub rate_denom: u32,
    /// Delay to (playback) or from (capture) the device, in ticks; can be negative.
    pub delay: i64,
    /// Frames inside the stream's resampler, at [`RATE`].
    pub buffered: u64,
}

impl Timing {
    /// The timing of the current cycle; `None` while the stream does not stream.
    /// Real-time safe.
    pub fn of(stream: &Stream) -> Option<Self> {
        let time = stream.time().ok()?;
        let rate = time.rate();
        (rate.num != 0 && rate.denom != 0).then(|| Self {
            now_ns: time.now(),
            rate_num: rate.num,
            rate_denom: rate.denom,
            delay: time.delay(),
            buffered: time.buffered(),
        })
    }

    /// Nanoseconds per tick.
    pub fn tick_ns(&self) -> f64 {
        1e9 * f64::from(self.rate_num) / f64::from(self.rate_denom)
    }
}

/// Duration of `frames` frames at [`RATE`], in ns.
pub(super) fn frames_to_ns(frames: u64) -> f64 {
    frames as f64 * 1e9 / f64::from(RATE)
}

/// `EnumFormat` with fixed values: F32LE, 48 kHz, stereo FL FR.
fn format_pod() -> Result<Vec<u8>, String> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(RATE);
    info.set_channels(CHANNELS as u32);
    let mut position = [0; spa::sys::SPA_AUDIO_MAX_CHANNELS as usize];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);
    let object = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(object),
    )
    .map(|(cursor, _)| cursor.into_inner())
    .map_err(|e| format!("cannot build the audio format: {e:?}"))
}

/// A connected capture or playback stream with real-time state `D`. Dropping it disconnects
/// the stream first, so no `process` call runs any more, then frees the callbacks and the
/// state, then destroys the stream.
pub(super) struct MediaStream<D> {
    /// Tags the state changes this stream reports.
    pub id: u64,
    process: Option<StreamListener<D>>,
    events: Option<StreamListener<()>>,
    stream: StreamRc,
}

impl<D> MediaStream<D> {
    /// The stream, for timing queries in tests.
    #[cfg(test)]
    pub fn stream(&self) -> &Stream {
        &self.stream
    }
}

impl<D> Drop for MediaStream<D> {
    fn drop(&mut self) {
        // Disconnecting removes the node from the data loop. Removing the process listener
        // also waits for the data loop (PipeWire clears its RT callbacks under the loop lock).
        let _ = self.stream.disconnect();
        self.process.take();
        self.events.take();
    }
}

/// What a new stream is.
pub(super) struct StreamSpec<'a> {
    pub name: &'a str,
    pub properties: PropertiesBox,
    pub direction: Direction,
    pub id: u64,
}

/// Creates and connects a stream. `process` runs on the real-time thread with `state`;
/// `on_state` gets every new state on the PipeWire thread.
pub(super) fn connect<D: 'static>(
    core: &CoreRc,
    spec: StreamSpec<'_>,
    state: D,
    process: fn(&Stream, &mut D),
    mut on_state: impl FnMut(StreamState) + 'static,
) -> Result<MediaStream<D>, String> {
    let stream = StreamRc::new(core.clone(), spec.name, spec.properties)
        .map_err(|e| format!("cannot create the stream: {e}"))?;
    // Two listeners, so the real-time state is never reachable from the other callbacks.
    let process_listener = stream
        .add_local_listener_with_user_data(state)
        .process(process)
        .register()
        .map_err(|e| format!("cannot listen to the stream: {e}"))?;
    let events_listener = stream
        .add_local_listener_with_user_data(())
        .state_changed(move |_, _, _old, new| on_state(new))
        .register()
        .map_err(|e| format!("cannot listen to the stream: {e}"))?;
    let format = format_pod()?;
    let pod = Pod::from_bytes(&format).ok_or("cannot read the audio format")?;
    let flags = StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS;
    let handle = MediaStream {
        id: spec.id,
        process: Some(process_listener),
        events: Some(events_listener),
        stream,
    };
    handle
        .stream
        .connect(spec.direction, None, flags, &mut [pod])
        .map_err(|e| format!("cannot connect the stream: {e}"))?;
    Ok(handle)
}
