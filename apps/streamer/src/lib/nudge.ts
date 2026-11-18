export type NudgeStep = 10 | 50 | 100;

export const DEFAULT_NUDGE_STEP: NudgeStep = 50;

/** The web client shows the negated nudge: a positive nudge plays earlier and reads "-50ms". */
export function nudgeLabel(nudgeOffsetMs: number): string {
  const shown = -nudgeOffsetMs;
  return `${shown >= 0 ? "+" : ""}${shown}ms`;
}

export function nextNudgeStep(step: NudgeStep): NudgeStep {
  return step === 10 ? 50 : step === 50 ? 100 : 10;
}
