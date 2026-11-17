import { LIVE_STREAM_CONSTANTS } from "@beatsync/shared";

const { SAMPLE_RATE, FRAME_DURATION_MS } = LIVE_STREAM_CONSTANTS;
const FRAME_DURATION_US = FRAME_DURATION_MS * 1000;

export interface DecodedBlock {
  sequence: number;
  frames: number;
  channelData: Float32Array[];
}

export interface OpusDecoderHandle {
  readonly kind: "webcodecs" | "wasm";
  decode(sequence: number, packet: Uint8Array): void;
  close(): void;
}

interface CreateOpusDecoderOptions {
  channels: 1 | 2;
  /** Skip WebCodecs, e.g. after it claimed Opus support but failed at configure or decode time */
  forceWasm?: boolean;
  onBlock: (block: DecodedBlock) => void;
  onError: (error: Error) => void;
}

/** Opus decoder for live frames: WebCodecs when the browser supports Opus and forceWasm is off, else WASM. */
export async function createOpusDecoder(o: CreateOpusDecoderOptions): Promise<OpusDecoderHandle> {
  if (!o.forceWasm && (await isWebCodecsOpusSupported(o.channels))) return createWebCodecsDecoder(o);
  return createWasmDecoder(o);
}

async function isWebCodecsOpusSupported(channels: number): Promise<boolean> {
  if (typeof AudioDecoder === "undefined") return false;
  try {
    const { supported } = await AudioDecoder.isConfigSupported({
      codec: "opus",
      sampleRate: SAMPLE_RATE,
      numberOfChannels: channels,
    });
    return supported === true;
  } catch {
    return false;
  }
}

/** The WebCodecs classes the decoder needs; tests pass fakes because happy-dom has no WebCodecs */
export interface WebCodecsAudioApi {
  AudioDecoder: new (init: AudioDecoderInit) => Pick<AudioDecoder, "state" | "configure" | "decode" | "close">;
  EncodedAudioChunk: new (init: EncodedAudioChunkInit) => EncodedAudioChunk;
}

export function createWebCodecsDecoder(
  o: CreateOpusDecoderOptions,
  api: WebCodecsAudioApi = { AudioDecoder, EncodedAudioChunk }
): OpusDecoderHandle {
  // Sequences of the chunks inside the decoder, oldest first. Opus gives one output per chunk,
  // in decode order. The output timestamp cannot identify the chunk: Chromium stamps outputs
  // from the first input timestamp plus the frames decoded, so an input gap does not show.
  const pendingSequences: number[] = [];
  const decoder = new api.AudioDecoder({
    output: (audioData) => {
      const sequence = pendingSequences.shift() ?? Math.round(audioData.timestamp / FRAME_DURATION_US);
      try {
        const channelData: Float32Array[] = [];
        for (let c = 0; c < audioData.numberOfChannels; c++) {
          const plane = new Float32Array(audioData.numberOfFrames);
          audioData.copyTo(plane, { planeIndex: c, format: "f32-planar" });
          channelData.push(plane);
        }
        o.onBlock({ sequence, frames: audioData.numberOfFrames, channelData });
      } finally {
        audioData.close();
      }
    },
    error: (e) => {
      pendingSequences.length = 0;
      o.onError(e);
    },
  });
  decoder.configure({ codec: "opus", sampleRate: SAMPLE_RATE, numberOfChannels: o.channels });

  return {
    kind: "webcodecs",
    decode(sequence, packet) {
      if (decoder.state !== "configured") return;
      const chunk = new api.EncodedAudioChunk({ type: "key", timestamp: sequence * FRAME_DURATION_US, data: packet });
      pendingSequences.push(sequence);
      decoder.decode(chunk);
    },
    close() {
      pendingSequences.length = 0;
      if (decoder.state !== "closed") decoder.close();
    },
  };
}

// Loaded on demand so the WASM decoder stays out of the main bundle. Next.js replaces
// `typeof window` at build time, so the SSR bundle drops this import: there, opus-decoder
// resolves the Node build of its worker shim, whose dynamic import breaks the build.
const loadOpusDecoderModule = () =>
  typeof window !== "undefined"
    ? import("opus-decoder")
    : Promise.reject(new Error("The WASM Opus decoder needs a browser"));

async function createWasmDecoder(o: CreateOpusDecoderOptions): Promise<OpusDecoderHandle> {
  const { OpusDecoder } = await loadOpusDecoderModule();
  const decoder = new OpusDecoder({ channels: o.channels, preSkip: 0, sampleRate: SAMPLE_RATE });
  await decoder.ready;
  let closed = false;

  return {
    kind: "wasm",
    decode(sequence, packet) {
      if (closed) return;
      let decoded: ReturnType<typeof decoder.decodeFrame>;
      try {
        decoded = decoder.decodeFrame(packet);
      } catch (e) {
        o.onError(e instanceof Error ? e : new Error(String(e)));
        return;
      }
      // Corrupt packets decode to 0 samples (the library logs the libopus error)
      if (decoded.samplesDecoded > 0) {
        o.onBlock({ sequence, frames: decoded.samplesDecoded, channelData: decoded.channelData });
      }
    },
    close() {
      if (closed) return;
      closed = true;
      decoder.free();
    },
  };
}
