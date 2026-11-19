import { describe, expect, test } from "bun:test";

import { DEFAULT_NUDGE_STEP, nextNudgeStep, nudgeLabel } from "./nudge";

describe("nudgeLabel", () => {
  test("shows the negated nudge, as the web client does", () => {
    expect(nudgeLabel(50)).toBe("-50ms");
    expect(nudgeLabel(-50)).toBe("+50ms");
    expect(nudgeLabel(0)).toBe("+0ms");
  });
});

describe("nextNudgeStep", () => {
  test("cycles 10 -> 50 -> 100 -> 10", () => {
    expect(nextNudgeStep(10)).toBe(50);
    expect(nextNudgeStep(50)).toBe(100);
    expect(nextNudgeStep(100)).toBe(10);
  });

  test("the default step is 50", () => {
    expect(DEFAULT_NUDGE_STEP).toBe(50);
  });
});
