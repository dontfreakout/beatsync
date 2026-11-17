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
  onBlock: (block: DecodedBlock) => void;
  onError: (error: Error) => void;
}

/** Opus decoder for live frames: WebCodecs when the browser supports Opus, else the WASM fallback. */
export async function createOpusDecoder(o: CreateOpusDecoderOptions): Promise<OpusDecoderHandle> {
  if (await isWebCodecsOpusSupported(o.channels)) return createWebCodecsDecoder(o);
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

function createWebCodecsDecoder(o: CreateOpusDecoderOptions): OpusDecoderHandle {
  const decoder = new AudioDecoder({
    output: (audioData) => {
      try {
        const channelData: Float32Array[] = [];
        for (let c = 0; c < audioData.numberOfChannels; c++) {
          const plane = new Float32Array(audioData.numberOfFrames);
          audioData.copyTo(plane, { planeIndex: c, format: "f32-planar" });
          channelData.push(plane);
        }
        o.onBlock({
          // The decoder carries the chunk timestamp through, so it maps back to the sequence
          sequence: Math.round(audioData.timestamp / FRAME_DURATION_US),
          frames: audioData.numberOfFrames,
          channelData,
        });
      } finally {
        audioData.close();
      }
    },
    error: (e) => o.onError(e),
  });
  decoder.configure({ codec: "opus", sampleRate: SAMPLE_RATE, numberOfChannels: o.channels });

  return {
    kind: "webcodecs",
    decode(sequence, packet) {
      if (decoder.state !== "configured") return;
      decoder.decode(new EncodedAudioChunk({ type: "key", timestamp: sequence * FRAME_DURATION_US, data: packet }));
    },
    close() {
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
