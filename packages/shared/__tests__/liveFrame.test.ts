import { describe, expect, it } from "bun:test";
import vectors from "../fixtures/live-frame-vectors.json";
import { decodeLiveFrame, encodeLiveFrame } from "../liveFrame";

const hexToBytes = (hex: string): Uint8Array => {
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  return out;
};
const bytesToHex = (bytes: Uint8Array): string => Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");

describe("live frame codec", () => {
  it("decodes every fixture vector", () => {
    for (const v of vectors) {
      expect(decodeLiveFrame(hexToBytes(v.hex))).toEqual({
        header: { streamId: v.streamId, sequence: v.sequence, captureServerTimeMs: v.captureServerTimeMs },
        payload: hexToBytes(v.payloadHex),
      });
    }
  });

  it("encodes every fixture vector byte-exactly", () => {
    for (const v of vectors) {
      const header = { streamId: v.streamId, sequence: v.sequence, captureServerTimeMs: v.captureServerTimeMs };
      expect(bytesToHex(encodeLiveFrame(header, hexToBytes(v.payloadHex)))).toBe(v.hex);
    }
  });

  it("decodes a header inside a larger buffer at a non-zero byteOffset", () => {
    const v = vectors[0]!;
    const frame = hexToBytes(v.hex);
    const big = new Uint8Array(frame.length + 10);
    big.set(frame, 3);
    const view = new Uint8Array(big.buffer, 3, frame.length);
    expect(decodeLiveFrame(view)).toEqual({
      header: { streamId: v.streamId, sequence: v.sequence, captureServerTimeMs: v.captureServerTimeMs },
      payload: hexToBytes(v.payloadHex),
    });
  });

  it("rejects short or foreign frames", () => {
    expect(decodeLiveFrame(new Uint8Array(16))).toBeNull();
    const foreign = hexToBytes(vectors[0]!.hex);
    foreign[0] = 2;
    expect(decodeLiveFrame(foreign)).toBeNull();
  });
});
