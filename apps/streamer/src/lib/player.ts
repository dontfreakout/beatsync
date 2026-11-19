// Pure player logic, ported from the web client (`store/global.tsx`, `SearchResults.tsx`).
import type { TrackType, WSRequestType } from "@beatsync/shared";

import type { PlaybackState } from "@/store/room";

/** The server clock, or null while the core has no offset. `offsetMs` is server minus local. */
export function serverNowMs(offsetMs: number | null, now = Date.now()): number | null {
  return offsetMs === null ? null : now + offsetMs;
}

/**
 * The track position in seconds. A PLAY runs from `serverTimeToExecute`; before that time it
 * stays at its start. A PAUSE keeps its position. With no playback the position is 0.
 */
export function trackPosition(playback: PlaybackState | null, serverNow: number | null, durationS: number | null): number {
  if (!playback) return 0;
  const start = playback.trackTimeSeconds ?? 0;
  let position = start;
  if (playback.kind === "PLAY" && serverNow !== null) {
    position = start + Math.max(0, serverNow - playback.serverTimeToExecute) / 1000;
  }
  if (durationS !== null && durationS > 0) position = Math.min(position, durationS);
  return Math.max(0, position);
}

export function isPlaying(playback: PlaybackState | null, serverNow: number | null, durationS: number | null): boolean {
  if (playback?.kind !== "PLAY") return false;
  if (durationS === null || durationS <= 0) return true;
  return trackPosition(playback, serverNow, durationS) < durationS;
}

/** The one "playing" rule of the player and the queue: the playback is a running PLAY of the selected track. */
export function isSelectedPlaying(
  playback: PlaybackState | null,
  selected: string | null,
  serverNow: number | null,
  durationS: number | null
): boolean {
  return selected !== null && playback?.audioSource === selected && isPlaying(playback, serverNow, durationS);
}

/** What the server set for the playback; it stays the same while the position runs on. */
export function playbackKey(playback: PlaybackState | null): string {
  if (!playback) return "none";
  return `${playback.kind}|${playback.audioSource ?? ""}|${playback.serverTimeToExecute}|${playback.trackTimeSeconds ?? ""}`;
}

/**
 * Sends a seek and holds its position on the seek bar (see `heldSeekPosition`). A seek that is
 * not sent (for example, the socket is closed) releases its hold, unless a newer one replaced it.
 */
export async function sendHeldSeek<H>(
  hold: H,
  setHeld: (update: (current: H | null) => H | null) => void,
  send: () => Promise<boolean>
): Promise<void> {
  setHeld(() => hold);
  if (!(await send())) setHeld((current) => (current === hold ? null : current));
}

/** A committed seek position stays on screen until the server sets another playback. */
export function heldSeekPosition(held: { position: number; key: string } | null, key: string): number | null {
  return held !== null && held.key === key ? held.position : null;
}

/** Web `skipToNextTrack`: null for one track or no current track; shuffle never repeats. */
export function nextTrack(urls: string[], current: string | null, shuffled: boolean, rand = Math.random): string | null {
  if (urls.length <= 1 || current === null) return null;
  const index = urls.indexOf(current);
  if (index < 0) return null;
  if (!shuffled) return urls[(index + 1) % urls.length];
  let next: number;
  do {
    next = Math.floor(rand() * urls.length);
  } while (next === index);
  return urls[next];
}

/** Web `skipToPreviousTrack`: wraps, ignores shuffle, no restart rule. */
export function previousTrack(urls: string[], current: string | null): string | null {
  if (urls.length === 0 || current === null) return null;
  const index = urls.indexOf(current);
  if (index < 0) return null;
  return urls[(index - 1 + urls.length) % urls.length];
}

/** "MM:SS" as the web: minutes are not capped; 0, NaN and negative give "00:00". */
export function formatTime(seconds: number): string {
  if (!seconds || Number.isNaN(seconds) || seconds < 0) return "00:00";
  const minutes = Math.floor(seconds / 60);
  const rest = Math.floor(seconds % 60);
  return `${String(minutes).padStart(2, "0")}:${String(rest).padStart(2, "0")}`;
}

/** "Artist 1, Artist 2 - Title (Version)", as `SearchResults.tsx`. */
export function formatTrackName(track: TrackType): string {
  const artists: string[] = [];
  if (track.performer?.name) artists.push(track.performer.name);
  for (const artist of track.album?.artists ?? []) {
    if (artist.name && !artists.includes(artist.name)) artists.push(artist.name);
  }
  const artistText = artists.length > 0 ? artists.join(", ") : "Unknown Artist";
  const title = (track.title || "Unknown Title").trim();
  const version = track.version?.trim();
  return `${artistText} - ${version ? `${title} (${version})` : title}`;
}

/**
 * The position shown for the selected track. A track that the playback does not name shows 0, or
 * the seek made while paused: never the position of another track.
 */
export function selectedPosition(args: {
  playback: PlaybackState | null;
  selected: string | null;
  pausedPosition: number | null;
  serverNow: number | null;
  durationS: number | null;
}): number {
  const { playback, selected, pausedPosition, serverNow, durationS } = args;
  if (!playback || selected === null || playback.audioSource !== selected) return pausedPosition ?? 0;
  if (isPlaying(playback, serverNow, durationS)) return trackPosition(playback, serverNow, durationS);
  return pausedPosition ?? trackPosition(playback, serverNow, durationS);
}

/** Web previous/next: a skip starts the track at 0 only while playing; else it changes the local selection only. */
export function skipAction(url: string, playing: boolean): WSRequestType | null {
  return playing ? { type: "PLAY", audioSource: url, trackTimeSeconds: 0 } : null;
}

/** Web seek commit: while playing it restarts at the position; while paused it stays local. */
export function seekAction(url: string, playing: boolean, position: number): WSRequestType | null {
  return playing ? { type: "PLAY", audioSource: url, trackTimeSeconds: position } : null;
}
