// Typed wrappers of the Tauri commands in src-tauri/src/commands.rs. The shapes follow the
// serde output of streamer-core (camelCase fields, externally tagged `Phase`).
import { Channel, invoke } from "@tauri-apps/api/core";

export interface Settings {
  serverUrl: string;
  roomId: string;
  username: string;
  clientId: string;
  selectedApps: string[];
  delayMs: number;
  localOffsetMs: number;
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
  rttMs: number | null;
  listeners: number;
  phase: Phase;
  message: string | null;
}

export const getSettings = () => invoke<Settings>("get_settings");

/** Returns the settings as saved (delay clamped to 250..5000 ms). */
export const saveSettings = (settings: Settings) => invoke<Settings>("save_settings", { settings });

/** Rejects with the backend error, e.g. "PipeWire 1.0 and WirePlumber 0.5 or later required". */
export const listApps = () => invoke<AppInfo[]>("list_apps");

export const startStream = (apps: string[]) => invoke<void>("start_stream", { apps });

export const updateApps = (apps: string[]) => invoke<void>("update_apps", { apps });

/** Resolves when the routing is restored. */
export const stopStream = () => invoke<void>("stop_stream");

/** Calls `onStatus` with the current status at once and then with every change. */
export function subscribeStatus(onStatus: (status: Status) => void): Promise<void> {
  const channel = new Channel<Status>(onStatus);
  return invoke<void>("subscribe_status", { onStatus: channel });
}

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
