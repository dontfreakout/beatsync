import { getWsUrl } from "@/lib/urls";
import { encodeLiveFrame, epochNow, LIVE_STREAM_CONSTANTS } from "@beatsync/shared";
import { nanoid } from "nanoid";

// Dev-only: acts as a streamer client. Generates PCM, encodes Opus, sends live frames.

const { SAMPLE_RATE, FRAME_DURATION_MS, FRAME_SAMPLES } = LIVE_STREAM_CONSTANTS;
const CHANNELS = 2;
const WARMUP_SAMPLES = 16;
const PROBE_SPACING_MS = 50;
const PROBE_INTERVAL_MS = 2500;
const CLICK_PERIOD = SAMPLE_RATE / 2; // 500 ms
const CLICK_LENGTH = SAMPLE_RATE / 100; // 10 ms

type Audio = { kind: "click" } | { kind: "file"; file: File };
type Pcm = { length: number; channels: Float32Array[] };
type Sample = { offset: number; rtt: number };
type Options = { roomId: string; username: string; delayMs: number; onStatus: (s: string) => void };

function clickPcm(): Pcm {
  const data = new Float32Array(CLICK_PERIOD);
  for (let i = 0; i < CLICK_LENGTH; i++) data[i] = 0.8 * Math.sin((2 * Math.PI * 1000 * i) / SAMPLE_RATE);
  return { length: CLICK_PERIOD, channels: [data, data] };
}

async function filePcm(file: File): Promise<Pcm> {
  const ctx = new OfflineAudioContext(CHANNELS, 1, SAMPLE_RATE);
  const buf = await ctx.decodeAudioData(await file.arrayBuffer());
  const left = buf.getChannelData(0);
  const right = buf.numberOfChannels > 1 ? buf.getChannelData(1) : left;
  return { length: buf.length, channels: [left, right] };
}

export class DevLiveSource {
  private ws: WebSocket | null = null;
  private encoder: AudioEncoder | null = null;
  private pcm: Pcm | null = null;
  private label = "Dev click track";
  private readonly clientId = `dev-source-${nanoid()}`;
  private samples: Sample[] = [];
  private offset = 0;
  private probeCount = 0;
  private probeTimer: ReturnType<typeof setInterval> | null = null;
  private encodeTimer: ReturnType<typeof setInterval> | null = null;
  private streamRequested = false;
  private streamId: number | null = null;
  private t0 = 0;
  private nextFrame = 0;
  private stopped = false;

  constructor(private readonly o: Options) {}

  async start(audio: Audio): Promise<void> {
    if (audio.kind === "file") {
      this.label = audio.file.name;
      this.pcm = await filePcm(audio.file);
    } else {
      this.label = "Dev click track";
      this.pcm = clickPcm();
    }
    const params = new URLSearchParams({
      roomId: this.o.roomId,
      username: this.o.username,
      clientId: this.clientId,
      kind: "streamer",
    });
    const ws = new WebSocket(`${getWsUrl()}?${params.toString()}`);
    ws.binaryType = "arraybuffer";
    this.ws = ws;
    this.o.onStatus("connecting");
    ws.onopen = () => {
      this.o.onStatus("syncing clock");
      this.sendProbe();
      this.probeTimer = setInterval(() => this.sendProbe(), PROBE_SPACING_MS);
    };
    ws.onmessage = (ev) => {
      if (typeof ev.data === "string") this.onMessage(ev.data);
    };
    ws.onclose = () => {
      if (!this.stopped) this.o.onStatus("socket closed");
      this.cleanup();
    };
    ws.onerror = () => this.o.onStatus("socket error");
  }

  stop(): void {
    this.stopped = true;
    if (this.ws?.readyState === WebSocket.OPEN) this.ws.send(JSON.stringify({ type: "STOP_LIVE_STREAM" }));
    this.cleanup();
  }

  private serverNow(): number {
    return epochNow() + this.offset;
  }

  private sendProbe(): void {
    if (this.ws?.readyState !== WebSocket.OPEN) return;
    const id = this.probeCount++;
    this.ws.send(JSON.stringify({ type: "NTP_REQUEST", t0: epochNow(), probeGroupId: id, probeGroupIndex: 0 }));
    // Switch to the slow cadence after the warm-up burst.
    if (this.probeCount === WARMUP_SAMPLES && this.probeTimer) {
      clearInterval(this.probeTimer);
      this.probeTimer = setInterval(() => this.sendProbe(), PROBE_INTERVAL_MS);
    }
  }

  private onMessage(raw: string): void {
    let msg: { type?: string; [k: string]: unknown };
    try {
      msg = JSON.parse(raw);
    } catch {
      return;
    }
    switch (msg.type) {
      case "LIVENESS_PING":
        this.ws?.send(JSON.stringify({ type: "LIVENESS_PONG" }));
        break;
      case "NTP_RESPONSE":
        this.onNtp(msg as unknown as { t0: number; t1: number; t2: number });
        break;
      case "LIVE_STREAM_REJECTED":
        this.o.onStatus(`rejected: ${String(msg.reason)}`);
        break;
      case "ROOM_EVENT": {
        const event = msg.event as
          | { type?: string; stream?: { streamId: number; sourceClientId: string } | null }
          | undefined;
        if (event?.type === "LIVE_STREAM_STATE") this.onStreamState(event.stream ?? null);
        break;
      }
    }
  }

  private onNtp({ t0, t1, t2 }: { t0: number; t1: number; t2: number }): void {
    const t3 = epochNow();
    this.samples.push({ offset: (t1 - t0 + (t2 - t3)) / 2, rtt: t3 - t0 - (t2 - t1) });
    if (this.samples.length > WARMUP_SAMPLES) this.samples.shift();
    this.offset = this.samples.reduce((best, s) => (s.rtt < best.rtt ? s : best)).offset;
    if (this.samples.length >= WARMUP_SAMPLES && !this.streamRequested) {
      this.streamRequested = true;
      this.o.onStatus("requesting stream");
      this.ws?.send(
        JSON.stringify({
          type: "START_LIVE_STREAM",
          codec: "opus",
          sampleRate: SAMPLE_RATE,
          channels: CHANNELS,
          frameDurationMs: FRAME_DURATION_MS,
          delayMs: this.o.delayMs,
          label: this.label,
        })
      );
    }
  }

  private onStreamState(stream: { streamId: number; sourceClientId: string } | null): void {
    if (stream && stream.sourceClientId === this.clientId) {
      if (this.streamId !== stream.streamId) {
        this.streamId = stream.streamId;
        this.beginSending();
      }
    } else if (this.streamId !== null) {
      this.streamId = null;
      this.o.onStatus("stream ended");
      this.stopSending();
    }
  }

  private beginSending(): void {
    this.t0 = this.serverNow() + 100;
    this.nextFrame = 0;
    this.encoder = new AudioEncoder({
      output: (chunk) => this.onEncoded(chunk),
      error: (e) => this.o.onStatus(`encoder error: ${e.message}`),
    });
    this.encoder.configure({ codec: "opus", sampleRate: SAMPLE_RATE, numberOfChannels: CHANNELS, bitrate: 128000 });
    this.encodeTimer = setInterval(() => this.encodeDue(), 10);
    this.o.onStatus("streaming");
  }

  private stopSending(): void {
    if (this.encodeTimer) clearInterval(this.encodeTimer);
    this.encodeTimer = null;
    if (this.encoder && this.encoder.state !== "closed") this.encoder.close();
    this.encoder = null;
  }

  private encodeDue(): void {
    const enc = this.encoder;
    const pcm = this.pcm;
    if (!enc || enc.state !== "configured" || !pcm) return;
    const now = this.serverNow();
    while (this.t0 + FRAME_DURATION_MS * (this.nextFrame + 1) <= now) {
      const n = this.nextFrame++;
      const planar = new Float32Array(FRAME_SAMPLES * CHANNELS);
      const start = n * FRAME_SAMPLES;
      for (let c = 0; c < CHANNELS; c++) {
        const src = pcm.channels[c]!;
        for (let i = 0; i < FRAME_SAMPLES; i++) planar[c * FRAME_SAMPLES + i] = src[(start + i) % pcm.length]!;
      }
      const data = new AudioData({
        format: "f32-planar",
        sampleRate: SAMPLE_RATE,
        numberOfFrames: FRAME_SAMPLES,
        numberOfChannels: CHANNELS,
        timestamp: n * FRAME_DURATION_MS * 1000,
        data: planar,
      });
      enc.encode(data);
      data.close();
    }
  }

  private onEncoded(chunk: EncodedAudioChunk): void {
    if (this.streamId === null || this.ws?.readyState !== WebSocket.OPEN) return;
    const bytes = new Uint8Array(new ArrayBuffer(chunk.byteLength));
    chunk.copyTo(bytes);
    const n = Math.round(chunk.timestamp / (FRAME_DURATION_MS * 1000));
    const frame = encodeLiveFrame(
      { streamId: this.streamId, sequence: n, captureServerTimeMs: this.t0 + FRAME_DURATION_MS * n },
      bytes
    ) as Uint8Array<ArrayBuffer>; // encodeLiveFrame allocates a plain ArrayBuffer
    this.ws.send(frame);
  }

  private cleanup(): void {
    if (this.probeTimer) clearInterval(this.probeTimer);
    this.probeTimer = null;
    this.stopSending();
    const ws = this.ws;
    this.ws = null;
    if (ws && ws.readyState <= WebSocket.OPEN) ws.close();
  }
}
