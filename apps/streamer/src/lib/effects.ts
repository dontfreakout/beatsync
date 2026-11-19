// Pure helpers for the effect controls, ported from the web client.
import { GRID, LOW_PASS_CONSTANTS } from "@beatsync/shared";

const LOG_MIN = Math.log(LOW_PASS_CONSTANTS.MIN_FREQ);
const LOG_MAX = Math.log(LOW_PASS_CONSTANTS.MAX_FREQ);

/** Log scale of `LowPassControl.tsx`: slider 0..100 to Hz. */
export function sliderToFreq(slider: number): number {
  return Math.round(Math.exp(LOG_MIN + (slider / 100) * (LOG_MAX - LOG_MIN)));
}

export function freqToSlider(freq: number): number {
  return ((Math.log(freq) - LOG_MIN) / (LOG_MAX - LOG_MIN)) * 100;
}

export function lowPassLabel(freq: number): string {
  if (freq >= 19000) return "OFF";
  if (freq >= 1000) return `${(freq / 1000).toFixed(1)}k`;
  return String(Math.round(freq));
}

/** The meter colour of `SpatialGainMeter.tsx`. */
export function gainColor(gain: number): string {
  if (gain >= 0.9) return "#22c55e";
  if (gain >= 0.7) return "#4ade80";
  if (gain >= 0.5) return "#86efac";
  if (gain >= 0.3) return "#a3a3a3";
  return "#737373";
}

export function volumeIcon(volume: number): "x" | "low" | "high" {
  if (volume === 0) return "x";
  return volume < 0.5 ? "low" : "high";
}

/** Web mute button: a sound volume goes to 0, mute goes to 0.5. */
export function toggleMuteVolume(volume: number): number {
  return volume > 0 ? 0 : 0.5;
}

/** The grid position (0..100 in x and y) under a pointer, rounded and clamped, as in `UserGrid.tsx`. */
export function pointerToGrid(
  clientX: number,
  clientY: number,
  rect: { left: number; top: number; width: number; height: number }
): { x: number; y: number } {
  const clamp = (v: number) => Math.max(0, Math.min(GRID.SIZE, v));
  return {
    x: clamp(Math.round(((clientX - rect.left) / rect.width) * GRID.SIZE)),
    y: clamp(Math.round(((clientY - rect.top) / rect.height) * GRID.SIZE)),
  };
}

/** A value that the user committed and that the server has not answered yet. */
export interface Held<T> {
  value: T;
  /** The server value at the time of the commit. */
  base: T;
}

/** The committed value stays on screen until the server value differs from the one before the commit. */
export function resolveHeld<T>(held: Held<T> | null, server: T, equal: (a: T, b: T) => boolean): T {
  return held !== null && equal(held.base, server) ? held.value : server;
}

/** The hold after a server value: released once the server value differs from the one before the commit. */
export function nextHold<T>(held: Held<T> | null, server: T, equal: (a: T, b: T) => boolean): Held<T> | null {
  return held !== null && equal(held.base, server) ? held : null;
}

/** The web move handler returns while spatial audio is off: a press must not start a drag then. */
export function canStartDrag(canMutate: boolean, spatialEnabled: boolean): boolean {
  return canMutate && spatialEnabled;
}

/**
 * The point to send when a drag ends (pointer up or cancel): the last dragged point, or null when the
 * pointer never moved. The end event coordinates are not used (a cancel can report 0,0).
 */
export function dragOutcome<T>(lastMoved: T | null): T | null {
  return lastMoved;
}
