import { afterEach, beforeEach, describe, expect, it } from "bun:test";
import { createOpusDecoder, createWebCodecsDecoder, type WebCodecsAudioApi } from "../decoder";

const FRAME_DURATION_US = 20_000;

class FakeEncodedAudioChunk {
  readonly timestamp: number;
  constructor(init: EncodedAudioChunkInit) {
    this.timestamp = init.timestamp;
  }
}

/**
 * Stamps outputs like Chromium: from the first input timestamp plus the frames decoded
 * so far, so a gap in the input timestamps does not show in the output timestamps.
 */
class FakeAudioDecoder {
  static last: FakeAudioDecoder | null = null;
  state: CodecState = "unconfigured";
  private firstTimestamp: number | null = null;
  private decodedCount = 0;
  private emittedCount = 0;

  constructor(private readonly init: AudioDecoderInit) {
    FakeAudioDecoder.last = this;
  }

  configure(): void {
    this.state = "configured";
  }

  decode(chunk: EncodedAudioChunk): void {
    this.firstTimestamp ??= chunk.timestamp;
    this.decodedCount++;
  }

  /** Delivers the outputs of all chunks decoded so far (real outputs arrive asynchronously) */
  emitOutputs(): void {
    for (; this.emittedCount < this.decodedCount; this.emittedCount++) {
      const audioData = {
        timestamp: this.firstTimestamp! + this.emittedCount * FRAME_DURATION_US,
        numberOfFrames: 960,
        numberOfChannels: 2,
        copyTo: () => {},
        close: () => {},
      };
      this.init.output(audioData as unknown as AudioData);
    }
  }

  close(): void {
    this.state = "closed";
  }
}

const fakeApi = {
  AudioDecoder: FakeAudioDecoder,
  EncodedAudioChunk: FakeEncodedAudioChunk,
} as unknown as WebCodecsAudioApi;

describe("createWebCodecsDecoder", () => {
  it("reports the submitted sequences after an input gap, though output timestamps stay contiguous", () => {
    const sequences: number[] = [];
    const handle = createWebCodecsDecoder(
      { channels: 2, onBlock: (block) => sequences.push(block.sequence), onError: () => {} },
      fakeApi
    );
    for (const sequence of [100, 101, 102, 105, 106]) handle.decode(sequence, new Uint8Array([1]));
    FakeAudioDecoder.last!.emitOutputs();
    expect(sequences).toEqual([100, 101, 102, 105, 106]);
  });
});

describe("createOpusDecoder", () => {
  /** A browser whose WebCodecs claims Opus support (it may still fail at configure or decode time) */
  class OpusClaimingAudioDecoder extends FakeAudioDecoder {
    static isConfigSupported(): Promise<{ supported: boolean }> {
      return Promise.resolve({ supported: true });
    }
  }

  beforeEach(() => {
    Object.assign(globalThis, { AudioDecoder: OpusClaimingAudioDecoder, EncodedAudioChunk: FakeEncodedAudioChunk });
  });

  afterEach(() => {
    const g = globalThis as { AudioDecoder?: unknown; EncodedAudioChunk?: unknown };
    delete g.AudioDecoder;
    delete g.EncodedAudioChunk;
  });

  it("forceWasm opens the WASM decoder even when WebCodecs claims Opus support", async () => {
    const options = { channels: 2 as const, onBlock: () => {}, onError: () => {} };
    const preferred = await createOpusDecoder(options);
    expect(preferred.kind).toBe("webcodecs");
    preferred.close();

    const forced = await createOpusDecoder({ ...options, forceWasm: true });
    expect(forced.kind).toBe("wasm");
    forced.close();
  });
});
