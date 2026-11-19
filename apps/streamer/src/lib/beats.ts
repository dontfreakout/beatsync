// The metronome beat grid: 55 BPM on server time, beat 0 at server time 0.
export const BEAT_INTERVAL_MS = 60000 / 55;

/**
 * The index of the beat that is running at a server time, in ms. It divides by the rounded
 * interval exactly as the web client (`useBeatTiming`) does, so at some exact beat times it gives
 * the beat before (60000 ms gives 54, not 55). Keep it so: the flash must match the web devices.
 */
export function beatIndex(serverNowMs: number): number {
  return Math.floor(serverNowMs / BEAT_INTERVAL_MS);
}
