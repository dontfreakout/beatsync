export interface LiveSchedulerOptions {
  frameDurationSec: number;
  safetyMarginSec: number;
  correctionThresholdSec: number;
  resyncThresholdSec: number;
  rateDelta: number;
}

export const DEFAULT_LIVE_SCHEDULER_OPTIONS: LiveSchedulerOptions = {
  frameDurationSec: 0.02,
  safetyMarginSec: 0.02,
  correctionThresholdSec: 0.002,
  resyncThresholdSec: 0.05,
  rateDelta: 0.001,
};

export type LiveScheduleDecision =
  | { kind: "play"; startSec: number; playbackRate: number }
  | { kind: "drop"; reason: "late" | "stale" };

/** Chains decoded blocks back to back and trims drift with a small playback-rate change. */
export class LiveScheduler {
  private readonly options: LiveSchedulerOptions;
  private nextStartSec: number | null = null;
  private lastSequence: number | null = null;

  constructor(options: Partial<LiveSchedulerOptions> = {}) {
    this.options = { ...DEFAULT_LIVE_SCHEDULER_OPTIONS, ...options };
  }

  schedule(b: { sequence: number; idealStartSec: number; durationSec: number; nowSec: number }): LiveScheduleDecision {
    const { frameDurationSec, safetyMarginSec, correctionThresholdSec, resyncThresholdSec, rateDelta } = this.options;
    if (this.lastSequence !== null && b.sequence <= this.lastSequence) {
      return { kind: "drop", reason: "stale" };
    }

    let next = this.nextStartSec;
    let err = 0;
    if (next !== null && this.lastSequence !== null) {
      next += Math.max(0, b.sequence - this.lastSequence - 1) * frameDurationSec;
      err = b.idealStartSec - next;
      if (Math.abs(err) > resyncThresholdSec || next < b.nowSec + safetyMarginSec) {
        next = null;
      }
    }

    let startSec: number;
    let playbackRate: number;
    if (next === null) {
      if (b.idealStartSec < b.nowSec + safetyMarginSec) {
        this.nextStartSec = null;
        this.lastSequence = b.sequence;
        return { kind: "drop", reason: "late" };
      }
      startSec = b.idealStartSec;
      playbackRate = 1;
    } else {
      startSec = next;
      playbackRate = err > correctionThresholdSec ? 1 - rateDelta : err < -correctionThresholdSec ? 1 + rateDelta : 1;
    }

    this.nextStartSec = startSec + b.durationSec / playbackRate;
    this.lastSequence = b.sequence;
    return { kind: "play", startSec, playbackRate };
  }

  reset(): void {
    this.nextStartSec = null;
    this.lastSequence = null;
  }
}

/** Tracks the share of late frames in a sliding time window. */
export class LateFrameMonitor {
  private samples: { atMs: number; late: boolean }[] = [];

  constructor(
    private readonly windowMs = 10_000,
    private readonly threshold = 0.05,
    private readonly minSamples = 50
  ) {}

  record(late: boolean, nowMs: number): void {
    this.samples.push({ atMs: nowMs, late });
    this.prune(nowMs);
  }

  isTooSlow(nowMs: number): boolean {
    this.prune(nowMs);
    if (this.samples.length < this.minSamples) return false;
    const lateCount = this.samples.filter((s) => s.late).length;
    return lateCount / this.samples.length > this.threshold;
  }

  private prune(nowMs: number): void {
    const cutoff = nowMs - this.windowMs;
    let i = 0;
    while (i < this.samples.length && this.samples[i]!.atMs < cutoff) i++;
    if (i > 0) this.samples.splice(0, i);
  }
}
