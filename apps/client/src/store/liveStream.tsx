import type { LiveStreamType } from "@beatsync/shared";
import { create } from "zustand";

const PLAY_HERE_STORAGE_KEY = "beatsync-live-play-here";

export type LiveStreamStatus = "idle" | "waiting" | "playing";

interface LiveStreamState {
  stream: LiveStreamType | null;
  /** Whether this device plays the live stream; persisted per browser */
  playHere: boolean;
  status: LiveStreamStatus;
  /** Too many decoded blocks arrive too late to schedule */
  isTooSlow: boolean;
  setStream: (stream: LiveStreamType | null) => void;
  setPlayHere: (playHere: boolean) => void;
  /** Forget the stream (e.g. on disconnect); keeps playHere */
  reset: () => void;
}

function readPlayHere(): boolean {
  // Server render: skip Node's experimental localStorage global
  if (typeof window === "undefined") return true;
  try {
    return localStorage.getItem(PLAY_HERE_STORAGE_KEY) !== "false";
  } catch {
    return true;
  }
}

function writePlayHere(playHere: boolean): void {
  try {
    localStorage.setItem(PLAY_HERE_STORAGE_KEY, String(playHere));
  } catch {
    // Storage unavailable (private mode, SSR): keep the in-memory value only
  }
}

export const useLiveStreamStore = create<LiveStreamState>((set) => ({
  stream: null,
  playHere: readPlayHere(),
  status: "idle",
  isTooSlow: false,
  setStream: (stream) => set({ stream }),
  setPlayHere: (playHere) => {
    writePlayHere(playHere);
    set({ playHere });
  },
  reset: () => set({ stream: null, status: "idle", isTooSlow: false }),
}));
