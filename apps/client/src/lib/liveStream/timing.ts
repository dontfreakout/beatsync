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

export function canScheduleLiveAudio(p: { isSynced: boolean; contextState: string; playHere: boolean }): boolean {
  return p.isSynced && p.contextState === "running" && p.playHere;
}
