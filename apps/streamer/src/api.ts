// Typed wrappers of the Tauri commands in src-tauri/src/commands.rs. The shapes follow the
// serde output of streamer-core (camelCase fields, externally tagged `Phase`).
import type { WSRequestType } from "@beatsync/shared";
import { Channel, invoke } from "@tauri-apps/api/core";

export interface Settings {
  serverUrl: string;
  /** The web client's address; empty means "derive it from `serverUrl`". */
  webUrl: string;
  roomId: string;
  username: string;
  clientId: string;
  selectedApps: string[];
  delayMs: number;
  /** The web client's nudge: positive plays the local copy earlier. */
  nudgeMs: number;
}

export interface AppInfo {
  name: string;
  binary: string | null;
  isBrowser: boolean;
  streams: number;
}

export type Phase = "idle" | "starting" | { streaming: { streamId: number } };

export interface Status {
  connected: boolean;
  synced: boolean;
  /** Synced at least once on this connection; a start before that has no local copy. */
  syncedOnce: boolean;
  rttMs: number | null;
  offsetMs: number | null;
  /** Accepted NTP measurements, 0..16. */
  syncMeasurements: number;
  nudgeMs: number;
  listeners: number;
  phase: Phase;
  message: string | null;
}

export interface SessionInfo {
  settings: Settings;
  joined: boolean;
}

export const getSession = () => invoke<SessionInfo>("get_session");

/** Returns the settings as saved (delay clamped to 250..5000 ms). */
export const saveSettings = (settings: Settings) => invoke<Settings>("save_settings", { settings });

/** Saves the room and the name and connects as kind `app`; returns the saved settings. Rejects with the backend error (bad room code, name or server URL; already joined). */
export const joinRoom = (roomId: string, username: string) => invoke<Settings>("join_room", { roomId, username });

/** Resolves when the routing is restored and the connection is closed. */
export const leaveRoom = () => invoke<void>("leave_room");

/** Saves the nudge, applies it at once and returns the saved settings. */
export const setNudge = (nudgeMs: number) => invoke<Settings>("set_nudge", { nudgeMs });

/** Sends a room action; rejects while disconnected or for a type the core sends itself. */
export const sendAction = (action: WSRequestType) => invoke<void>("send_action", { action });

/** Rejects with the backend error, e.g. "PipeWire 1.0 and WirePlumber 0.5 or later required". */
export const listApps = () => invoke<AppInfo[]>("list_apps");

/** Saves the selection, starts the stream and returns the saved settings. */
export const startStream = (apps: string[]) => invoke<Settings>("start_stream", { apps });

/** Saves the selection, changes the streamed apps and returns the saved settings. */
export const updateApps = (apps: string[]) => invoke<Settings>("update_apps", { apps });

/** Resolves when the routing is restored. */
export const stopStream = () => invoke<void>("stop_stream");

/** Calls `onStatus` with the current status at once and then with every change. */
export function subscribeStatus(onStatus: (status: Status) => void): Promise<void> {
  const channel = new Channel<Status>(onStatus);
  return invoke<void>("subscribe_status", { onStatus: channel });
}

/**
 * Calls `onMessage` with the cached room messages at once (one raw `@beatsync/shared`
 * WSResponse each) and then with every live room message.
 */
export function subscribeRoom(onMessage: (raw: unknown) => void): Promise<void> {
  const channel = new Channel<unknown>(onMessage);
  return invoke<void>("subscribe_room", { onMessage: channel });
}

/** The state of a queue track in the core, by the track URL as the server sends it. */
export type TrackState =
  | { state: "loading" }
  | { state: "ready"; durationS: number }
  | { state: "error"; message: string };

/** Calls `onTracks` with the track states at once (`{}` after a join or leave) and then with every change. */
export function subscribeTracks(onTracks: (tracks: Record<string, TrackState>) => void): Promise<void> {
  const channel = new Channel<Record<string, TrackState>>(onTracks);
  return invoke<void>("subscribe_tracks", { onTracks: channel });
}

/** Uploads the audio file at `path` to the room; rejects with the server's error text. */
export const uploadAudio = (path: string) => invoke<void>("upload_audio", { path });

/** The accepted NTP measurements at which the clock counts as synced. */
export const MAX_SYNC_MEASUREMENTS = 16;

export function isIdle(phase: Phase): boolean {
  return phase === "idle";
}

export function isStreaming(phase: Phase): phase is { streaming: { streamId: number } } {
  return typeof phase === "object" && "streaming" in phase;
}

/** Tauri rejects with the command's error string. */
export function errorText(error: unknown): string {
  return typeof error === "string" ? error : error instanceof Error ? error.message : String(error);
}
