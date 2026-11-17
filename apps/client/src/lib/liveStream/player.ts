import { audioContextManager } from "@/lib/audioContextManager";
import { createOpusDecoder, type DecodedBlock, type OpusDecoderHandle } from "@/lib/liveStream/decoder";
import { LateFrameMonitor, LiveScheduler } from "@/lib/liveStream/scheduler";
import { canScheduleLiveAudio, computeBlockIdealStartSec } from "@/lib/liveStream/timing";
import { getFilteredOutputLatencyMs, useGlobalStore } from "@/store/global";
import { type LiveStreamStatus, useLiveStreamStore } from "@/store/liveStream";
import { decodeLiveFrame, epochNow, LIVE_STREAM_CONSTANTS, type LiveStreamType } from "@beatsync/shared";

const { SAMPLE_RATE } = LIVE_STREAM_CONSTANTS;
const CAPTURE_TIME_RETENTION_MS = 5000;
const STATUS_INTERVAL_MS = 500;
const WAITING_GRACE_MS = 1000;
const DECODER_RETRY_MS = 1000;
/** WebCodecs can claim Opus support and still fail at configure or decode time */
const DECODER_FAILURES_BEFORE_WASM = 2;
/** getFilteredOutputLatencyMs warns on every call for Bluetooth outputs, so it is not read per block */
const OUTPUT_LATENCY_REFRESH_MS = 500;
/** A block that starts this much before the end of the queued audio would double it */
const OVERLAP_EPSILON_SEC = 0.001;

interface QueuedNode {
  node: AudioBufferSourceNode;
  startSec: number;
  /** AudioContext time at which the node stops sounding */
  endSec: number;
}

interface Session {
  stream: LiveStreamType;
  decoder: OpusDecoderHandle | null;
  scheduler: LiveScheduler;
  monitor: LateFrameMonitor;
  /** sequence → captureServerTimeMs, in arrival order */
  captureTimes: Map<number, number>;
  nodes: Set<QueuedNode>;
  /** AudioContext time at which the last scheduled node ends */
  queuedEndSec: number;
  lastFrameAt: number;
  statusTimer: ReturnType<typeof setInterval>;
  retryTimer: ReturnType<typeof setTimeout> | null;
  /** From DECODER_FAILURES_BEFORE_WASM on, the session uses the WASM decoder */
  decoderFailures: number;
}

export interface LiveStreamStats {
  decoderKind: string | null;
  received: number;
  decoded: number;
  played: number;
  droppedLate: number;
  droppedStale: number;
}

/** Decodes the room's live stream frames and plays them in sync, following useLiveStreamStore. */
class LiveStreamPlayer {
  private session: Session | null = null;
  private stats = { received: 0, decoded: 0, played: 0, droppedLate: 0, droppedStale: 0 };
  private outputLatencyMs = 0;
  private outputLatencyReadAt = -Infinity;

  constructor() {
    useLiveStreamStore.subscribe((state, prev) => {
      if (state.stream?.streamId !== prev.stream?.streamId || state.playHere !== prev.playHere) this.sync();
    });
    this.sync();
  }

  handleFrame(data: ArrayBuffer): void {
    this.stats.received++;
    const session = this.session;
    const decoder = session?.decoder;
    if (!session || !decoder) return;
    const frame = decodeLiveFrame(data);
    if (!frame || frame.header.streamId !== session.stream.streamId) return;

    const { sequence, captureServerTimeMs } = frame.header;
    session.captureTimes.set(sequence, captureServerTimeMs);
    for (const [seq, captureMs] of session.captureTimes) {
      if (captureMs >= captureServerTimeMs - CAPTURE_TIME_RETENTION_MS) break;
      session.captureTimes.delete(seq);
    }
    session.lastFrameAt = Date.now();
    decoder.decode(sequence, frame.payload);
  }

  getStats(): LiveStreamStats {
    return { decoderKind: this.session?.decoder?.kind ?? null, ...this.stats };
  }

  private sync(): void {
    const { stream, playHere } = useLiveStreamStore.getState();
    const wanted = stream && playHere ? stream : null;
    if (this.session && this.session.stream.streamId === wanted?.streamId) return;
    this.stop();
    if (wanted) this.start(wanted);
  }

  private start(stream: LiveStreamType): void {
    const session: Session = {
      stream,
      decoder: null,
      scheduler: new LiveScheduler(),
      monitor: new LateFrameMonitor(),
      captureTimes: new Map(),
      nodes: new Set(),
      queuedEndSec: 0,
      lastFrameAt: 0,
      statusTimer: setInterval(() => this.updateStatus(session), STATUS_INTERVAL_MS),
      retryTimer: null,
      decoderFailures: 0,
    };
    this.session = session;
    this.stats = { received: 0, decoded: 0, played: 0, droppedLate: 0, droppedStale: 0 };
    this.outputLatencyReadAt = -Infinity;
    this.updateStatus(session);
    void this.openDecoder(session);
  }

  private stop(): void {
    const session = this.session;
    if (!session) return;
    this.session = null;
    clearInterval(session.statusTimer);
    if (session.retryTimer) clearTimeout(session.retryTimer);
    this.stopNodes(session);
    session.decoder?.close();
    session.decoder = null;
    session.scheduler.reset();
    session.captureTimes.clear();
    setStatus({ status: "idle", isTooSlow: false });
  }

  private async openDecoder(session: Session): Promise<void> {
    let decoder: OpusDecoderHandle;
    try {
      decoder = await createOpusDecoder({
        channels: session.stream.channels,
        forceWasm: session.decoderFailures >= DECODER_FAILURES_BEFORE_WASM,
        onBlock: (block) => {
          if (this.session === session) this.handleBlock(session, block);
        },
        onError: (error) => this.handleDecoderError(session, error),
      });
    } catch (error) {
      this.handleDecoderError(session, error instanceof Error ? error : new Error(String(error)));
      return;
    }
    if (this.session !== session) {
      decoder.close();
      return;
    }
    session.decoder = decoder;
  }

  private handleDecoderError(session: Session, error: Error): void {
    if (this.session !== session || session.retryTimer) return;
    // Counts every failure: without WebCodecs Opus support the decoder is WASM already, so the switch changes nothing
    session.decoderFailures++;
    const next = session.decoderFailures >= DECODER_FAILURES_BEFORE_WASM ? " with the WASM decoder" : "";
    console.error(`[LiveStream] Decoder failed, retrying${next} in ${DECODER_RETRY_MS} ms`, error);
    session.decoder?.close();
    session.decoder = null;
    session.retryTimer = setTimeout(() => {
      session.retryTimer = null;
      if (this.session === session) void this.openDecoder(session);
    }, DECODER_RETRY_MS);
  }

  private handleBlock(session: Session, block: DecodedBlock): void {
    this.stats.decoded++;
    const captureServerTimeMs = session.captureTimes.get(block.sequence);
    if (captureServerTimeMs === undefined || block.frames === 0 || block.channelData.length === 0) return;

    const { isSynced, isInitingSystem, offsetEstimate, nudgeOffsetMs } = useGlobalStore.getState();
    const contextState = audioContextManager.getState() ?? "none";
    const playHere = useLiveStreamStore.getState().playHere;
    if (!canScheduleLiveAudio({ isSynced, isInitingSystem, contextState, playHere })) {
      // Queued audio is mistimed after e.g. a suspension: the next chain starts with nothing queued
      this.stopNodes(session);
      session.scheduler.reset();
      return;
    }

    const ctx = audioContextManager.getContext();
    const nowSec = ctx.currentTime;
    const durationSec = block.frames / SAMPLE_RATE;
    const idealStartSec = computeBlockIdealStartSec({
      captureServerTimeMs,
      delayMs: session.stream.delayMs,
      frameDurationMs: session.stream.frameDurationMs,
      blockFrames: block.frames,
      sampleRate: SAMPLE_RATE,
      nowEpochMs: epochNow(),
      nowCtxSec: nowSec,
      offsetEstimateMs: offsetEstimate,
      nudgeMs: nudgeOffsetMs,
      outputLatencyMs: this.readOutputLatencyMs(),
    });
    const decision = session.scheduler.schedule({ sequence: block.sequence, idealStartSec, durationSec, nowSec });
    session.monitor.record(decision.kind === "drop" && decision.reason === "late", Date.now());
    if (decision.kind === "drop") {
      if (decision.reason === "late") this.stats.droppedLate++;
      else this.stats.droppedStale++;
      return;
    }

    // A scheduler restart can move the chain earlier while old audio is still queued
    if (decision.startSec < session.queuedEndSec - OVERLAP_EPSILON_SEC) this.cutNodesAt(session, decision.startSec);

    const buffer = ctx.createBuffer(block.channelData.length, block.frames, SAMPLE_RATE);
    block.channelData.forEach((data, c) => buffer.getChannelData(c).set(data));
    const node = ctx.createBufferSource();
    node.buffer = buffer;
    node.playbackRate.value = decision.playbackRate;
    node.connect(audioContextManager.getInputNode());
    const queued: QueuedNode = {
      node,
      startSec: decision.startSec,
      endSec: decision.startSec + durationSec / decision.playbackRate,
    };
    node.onended = () => {
      session.nodes.delete(queued);
      node.disconnect();
    };
    node.start(queued.startSec);
    session.nodes.add(queued);
    session.queuedEndSec = queued.endSec;
    this.stats.played++;
  }

  /** Silences queued audio from atSec on, so the new chain follows the old one with no gap or overlap. */
  private cutNodesAt(session: Session, atSec: number): void {
    for (const queued of session.nodes) {
      if (queued.startSec >= atSec) {
        // Not started yet (atSec is in the future): it never sounds
        discardNode(session, queued);
      } else if (queued.endSec > atSec) {
        // A later stop() call replaces this one, so endSec tracks it to keep later cuts correct
        queued.node.stop(atSec);
        queued.endSec = atSec;
      }
    }
  }

  private stopNodes(session: Session): void {
    for (const queued of session.nodes) discardNode(session, queued);
    session.queuedEndSec = 0;
  }

  private updateStatus(session: Session): void {
    const now = Date.now();
    setStatus({
      status: now - session.lastFrameAt > session.stream.delayMs + WAITING_GRACE_MS ? "waiting" : "playing",
      isTooSlow: session.monitor.isTooSlow(now),
    });
  }

  private readOutputLatencyMs(): number {
    const now = Date.now();
    if (now - this.outputLatencyReadAt >= OUTPUT_LATENCY_REFRESH_MS) {
      this.outputLatencyMs = getFilteredOutputLatencyMs();
      this.outputLatencyReadAt = now;
    }
    return this.outputLatencyMs;
  }
}

function discardNode(session: Session, queued: QueuedNode): void {
  queued.node.onended = null;
  queued.node.stop();
  queued.node.disconnect();
  session.nodes.delete(queued);
}

function setStatus(next: { status: LiveStreamStatus; isTooSlow: boolean }): void {
  const { status, isTooSlow } = useLiveStreamStore.getState();
  if (status !== next.status || isTooSlow !== next.isTooSlow) useLiveStreamStore.setState(next);
}

export const liveStreamPlayer = new LiveStreamPlayer();

if (process.env.NODE_ENV === "development" && typeof window !== "undefined") {
  (window as Window & { __beatsyncLive?: LiveStreamPlayer }).__beatsyncLive = liveStreamPlayer;
}
