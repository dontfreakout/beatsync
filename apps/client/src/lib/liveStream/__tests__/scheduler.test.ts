import { describe, expect, it } from "bun:test";
import { LateFrameMonitor, LiveScheduler, type LiveScheduleDecision } from "../scheduler";

const D = 0.02;
const block = (sequence: number, idealStartSec: number, nowSec = 0) => ({
  sequence,
  idealStartSec,
  durationSec: D,
  nowSec,
});

function expectPlay(d: LiveScheduleDecision, startSec: number, playbackRate: number) {
  expect(d.kind).toBe("play");
  if (d.kind !== "play") return;
  expect(d.startSec).toBeCloseTo(startSec, 9);
  expect(d.playbackRate).toBe(playbackRate);
}

describe("LiveScheduler", () => {
  it("plays the first block at its ideal start with rate 1", () => {
    expectPlay(new LiveScheduler().schedule(block(0, 1)), 1, 1);
  });

  it("plays following blocks back to back", () => {
    const s = new LiveScheduler();
    let prevEnd = 1;
    for (let n = 0; n < 5; n++) {
      const d = s.schedule(block(n, 1 + D * n));
      expectPlay(d, n === 0 ? 1 : prevEnd, 1);
      prevEnd = (d.kind === "play" ? d.startSec : 0) + D;
    }
  });

  it("slows down when the ideal timeline is later than the chain", () => {
    const s = new LiveScheduler();
    s.schedule(block(0, 1.0));
    expectPlay(s.schedule(block(1, 1.023)), 1.02, 0.999);
  });

  it("speeds up when the ideal timeline is earlier than the chain", () => {
    const s = new LiveScheduler();
    s.schedule(block(0, 1.0));
    expectPlay(s.schedule(block(1, 1.017)), 1.02, 1.001);
  });

  it("restarts at the ideal time when the error exceeds 50 ms", () => {
    const s = new LiveScheduler();
    s.schedule(block(0, 1.0));
    expectPlay(s.schedule(block(1, 1.2)), 1.2, 1);
  });

  it("drops a block whose ideal start is inside the safety margin", () => {
    expect(new LiveScheduler().schedule(block(0, 1.01, 1.0))).toEqual({ kind: "drop", reason: "late" });
  });

  it("restarts after an underrun", () => {
    const s = new LiveScheduler();
    s.schedule(block(0, 1.0));
    expectPlay(s.schedule(block(1, 1.6, 1.5)), 1.6, 1);
  });

  it("advances the chain by the missing frames on a sequence gap", () => {
    const s = new LiveScheduler();
    s.schedule(block(0, 1.0));
    expectPlay(s.schedule(block(3, 1.06)), 1.06, 1);
  });

  it("drops a duplicate or older sequence", () => {
    const s = new LiveScheduler();
    s.schedule(block(5, 1.0));
    expect(s.schedule(block(5, 1.02))).toEqual({ kind: "drop", reason: "stale" });
    expect(s.schedule(block(4, 1.02))).toEqual({ kind: "drop", reason: "stale" });
  });

  it("accepts any sequence again after reset", () => {
    const s = new LiveScheduler();
    s.schedule(block(5, 1.0));
    s.reset();
    expectPlay(s.schedule(block(0, 2.0)), 2.0, 1);
  });
});

describe("LateFrameMonitor", () => {
  const feed = (m: LateFrameMonitor, total: number, late: number, atMs: number) => {
    for (let i = 0; i < total; i++) m.record(i < late, atMs);
  };

  it("does not flag exactly 5 % late", () => {
    const m = new LateFrameMonitor();
    feed(m, 60, 3, 1000);
    expect(m.isTooSlow(1000)).toBe(false);
  });

  it("flags more than 5 % late", () => {
    const m = new LateFrameMonitor();
    feed(m, 60, 4, 1000);
    expect(m.isTooSlow(1000)).toBe(true);
  });

  it("ignores records older than the window", () => {
    const m = new LateFrameMonitor();
    feed(m, 60, 4, 1000);
    expect(m.isTooSlow(11_001)).toBe(false);
  });

  it("needs at least 50 samples", () => {
    const m = new LateFrameMonitor();
    feed(m, 40, 10, 1000);
    expect(m.isTooSlow(1000)).toBe(false);
  });
});
