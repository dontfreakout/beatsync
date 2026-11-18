import { LIVE_STREAM_CONSTANTS } from "./constants";

// Binary live audio frame, little-endian:
// [0] kind (u8) | [1..5) streamId (u32) | [5..9) sequence (u32) | [9..17) captureServerTimeMs (f64) | payload
export interface LiveFrameHeader {
  streamId: number;
  sequence: number;
  captureServerTimeMs: number;
}

const { FRAME_HEADER_BYTES, FRAME_KIND_AUDIO } = LIVE_STREAM_CONSTANTS;

export function encodeLiveFrame(header: LiveFrameHeader, payload: Uint8Array): Uint8Array {
  const out = new Uint8Array(FRAME_HEADER_BYTES + payload.byteLength);
  const view = new DataView(out.buffer);
  view.setUint8(0, FRAME_KIND_AUDIO);
  view.setUint32(1, header.streamId, true);
  view.setUint32(5, header.sequence, true);
  view.setFloat64(9, header.captureServerTimeMs, true);
  out.set(payload, FRAME_HEADER_BYTES);
  return out;
}

export function decodeLiveFrame(data: ArrayBuffer | Uint8Array): { header: LiveFrameHeader; payload: Uint8Array } | null {
  const bytes = data instanceof Uint8Array ? data : new Uint8Array(data);
  if (bytes.byteLength < FRAME_HEADER_BYTES) return null;
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  if (view.getUint8(0) !== FRAME_KIND_AUDIO) return null;
  return {
    header: {
      streamId: view.getUint32(1, true),
      sequence: view.getUint32(5, true),
      captureServerTimeMs: view.getFloat64(9, true),
    },
    payload: bytes.subarray(FRAME_HEADER_BYTES),
  };
}
