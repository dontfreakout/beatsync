import type { LiveStreamType } from "@beatsync/shared";

export type LiveBarMode = "own-with-stop" | "own" | "other" | "none";

/**
 * Which live bar to show. `streamingHere` is this app's own phase. The server's stream can name
 * this PC while the phase is "starting" or already idle after a stop: show it as ours, without Stop.
 */
export function liveBarMode(streamingHere: boolean, stream: LiveStreamType | null, clientId: string): LiveBarMode {
  if (streamingHere) return "own-with-stop";
  if (stream === null) return "none";
  return stream.sourceClientId === clientId ? "own" : "other";
}
