import type { LiveStreamType } from "@beatsync/shared";
import { create } from "zustand";

interface LiveStreamState {
  stream: LiveStreamType | null;
  setStream: (stream: LiveStreamType | null) => void;
}

export const useLiveStreamStore = create<LiveStreamState>((set) => ({
  stream: null,
  setStream: (stream) => set({ stream }),
}));
