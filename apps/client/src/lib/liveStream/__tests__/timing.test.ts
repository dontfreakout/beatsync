import { describe, expect, it } from "bun:test";
import { calculateLocalDelayMs } from "@/utils/ntp";
import { canScheduleLiveAudio, computeBlockIdealStartSec } from "../timing";

describe("calculateLocalDelayMs", () => {
  const base = { targetServerTimeMs: 10_000, nowEpochMs: 9_000, offsetEstimateMs: 0, nudgeMs: 0, outputLatencyMs: 0 };

  it("uses the same sign for nudge as file playback", () => {
    expect(calculateLocalDelayMs(base)).toBe(1000);
    expect(calculateLocalDelayMs({ ...base, nudgeMs: 50 })).toBe(950);
    expect(calculateLocalDelayMs({ ...base, outputLatencyMs: 20 })).toBe(980);
    expect(calculateLocalDelayMs({ ...base, offsetEstimateMs: 100 })).toBe(900);
  });
});

describe("computeBlockIdealStartSec", () => {
  const base = {
    captureServerTimeMs: 5000,
    delayMs: 1000,
    frameDurationMs: 20,
    blockFrames: 960,
    sampleRate: 48000,
    nowEpochMs: 5500,
    nowCtxSec: 2,
    offsetEstimateMs: 0,
    nudgeMs: 0,
    outputLatencyMs: 0,
  };

  it("a full block is ideal-scheduled at capture + delay", () => {
    expect(computeBlockIdealStartSec(base)).toBeCloseTo(2.5, 9);
  });

  it("a block shortened by pre-skip ends where a full block would end", () => {
    expect(computeBlockIdealStartSec({ ...base, blockFrames: 648 })).toBeCloseTo(2.5 + 312 / 48000, 9);
  });
});

describe("canScheduleLiveAudio", () => {
  const ready = { isSynced: true, isInitingSystem: false, contextState: "running", playHere: true };

  it("requires synced, running context and playHere", () => {
    expect(canScheduleLiveAudio({ ...ready, isSynced: false })).toBe(false);
    expect(canScheduleLiveAudio({ ...ready, contextState: "suspended" })).toBe(false);
    expect(canScheduleLiveAudio({ ...ready, contextState: "interrupted" })).toBe(false);
    expect(canScheduleLiveAudio({ ...ready, playHere: false })).toBe(false);
    expect(canScheduleLiveAudio(ready)).toBe(true);
  });

  it("stays silent behind the Start screen even when the context runs", () => {
    expect(canScheduleLiveAudio({ ...ready, isInitingSystem: true })).toBe(false);
  });
});
