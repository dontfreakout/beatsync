// The saved settings, whether a room is joined, and the live status of the Rust core.
import { create } from "zustand";

import type { SessionInfo, Settings, Status, TrackState } from "@/api";

export const INITIAL_STATUS: Status = {
  connected: false,
  synced: false,
  syncedOnce: false,
  rttMs: null,
  offsetMs: null,
  syncMeasurements: 0,
  nudgeMs: 0,
  listeners: 0,
  phase: "idle",
  message: null,
};

export const DEFAULT_SETTINGS: Settings = {
  serverUrl: "",
  webUrl: "",
  roomId: "",
  username: "",
  clientId: "",
  selectedApps: [],
  delayMs: 1000,
  nudgeMs: 0,
};

interface SessionStore {
  settings: Settings;
  joined: boolean;
  status: Status;
  /** Queue track states from the core, by URL. Empty after a join or leave. */
  tracks: Record<string, TrackState>;
  /** Takes the session from `get_session`. */
  setSession: (session: SessionInfo) => void;
  /**
   * Takes the settings that `save_settings`, `join_room`, `set_nudge`, `start_stream` or
   * `update_apps` returned. They are the saved truth: a later save must not send a stale value
   * such as `nudgeMs`.
   */
  setSettings: (settings: Settings) => void;
  setJoined: (joined: boolean) => void;
  setStatus: (status: Status) => void;
  setTracks: (tracks: Record<string, TrackState>) => void;
}

export const useSessionStore = create<SessionStore>()((set) => ({
  settings: DEFAULT_SETTINGS,
  joined: false,
  status: INITIAL_STATUS,
  tracks: {},
  setSession: ({ settings, joined }) => set({ settings, joined }),
  setSettings: (settings) => set({ settings }),
  setJoined: (joined) => set({ joined }),
  setStatus: (status) => set({ status }),
  setTracks: (tracks) => set({ tracks }),
}));
