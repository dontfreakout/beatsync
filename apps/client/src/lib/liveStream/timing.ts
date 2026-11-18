import { calculateLocalDelayMs } from "@/utils/ntp";

/**
 * Ideal AudioContext start time of a decoded block, end-aligned: the block's end
 * falls on the capture time + delay + frame duration, so a first block that lost
 * Opus pre-skip samples stays aligned.
 */
export function computeBlockIdealStartSec(p: {
  captureServerTimeMs: number;
  delayMs: number;
  frameDurationMs: number;
  blockFrames: number;
  sampleRate: number;
  nowEpochMs: number;
  nowCtxSec: number;
  offsetEstimateMs: number;
  nudgeMs: number;
  outputLatencyMs: number;
}): number {
  const delayUntilEndMs = calculateLocalDelayMs({
    targetServerTimeMs: p.captureServerTimeMs + p.delayMs + p.frameDurationMs,
    nowEpochMs: p.nowEpochMs,
    offsetEstimateMs: p.offsetEstimateMs,
    nudgeMs: p.nudgeMs,
    outputLatencyMs: p.outputLatencyMs,
  });
  return p.nowCtxSec + delayUntilEndMs / 1000 - p.blockFrames / p.sampleRate;
}

/** Same gate as file playback (schedulePlay): no audio while the Start screen shows (isInitingSystem). */
export function canScheduleLiveAudio(p: {
  isSynced: boolean;
  isInitingSystem: boolean;
  contextState: string;
  playHere: boolean;
}): boolean {
  return p.isSynced && !p.isInitingSystem && p.contextState === "running" && p.playHere;
}

/**
 * A frame whose play time (capture + delay) passed more than one frame ago, e.g. in the
 * burst of buffered frames after a background tab resumes. Decoding it only yields a late drop.
 */
export function isFrameExpired(p: {
  captureServerTimeMs: number;
  delayMs: number;
  frameDurationMs: number;
  nowServerMs: number;
}): boolean {
  return p.nowServerMs - (p.captureServerTimeMs + p.delayMs) > p.frameDurationMs;
}
