// Local player state of the window: a selection made while paused, a seek while paused.
import { create } from "zustand";

import { type RoomState, useRoomStore } from "@/store/room";

interface PlayerStore {
  /** A skip while paused changes only this. Clear it whenever `room.currentAudioSource` changes. */
  localSelection: string | null;
  /** A seek while paused, in seconds. */
  pausedPosition: number | null;
  setLocalSelection: (url: string | null) => void;
  setPausedPosition: (seconds: number | null) => void;
  reset: () => void;
}

export const usePlayerStore = create<PlayerStore>()((set) => ({
  localSelection: null,
  pausedPosition: null,
  setLocalSelection: (localSelection) => set({ localSelection, pausedPosition: null }),
  setPausedPosition: (pausedPosition) => set({ pausedPosition }),
  reset: () => set({ localSelection: null, pausedPosition: null }),
}));

/**
 * The selected track: the local selection while paused, else the room's current source. A local
 * selection that is no longer in the queue is ignored.
 */
export const selectedUrl = (
  room: Pick<RoomState, "currentAudioSource" | "audioSources">,
  localSelection: string | null
): string | null =>
  localSelection !== null && room.audioSources.some((source) => source.url === localSelection)
    ? localSelection
    : room.currentAudioSource;

// The room decides: a new current track or a new PLAY/PAUSE ends the local choices.
useRoomStore.subscribe((room, previous) => {
  if (room.currentAudioSource !== previous.currentAudioSource) {
    usePlayerStore.setState({ localSelection: null, pausedPosition: null });
  } else if (room.playback !== previous.playback) {
    // A new PLAY starts the room's track: it overrides a local selection.
    usePlayerStore.setState(room.playback?.kind === "PLAY" ? { localSelection: null, pausedPosition: null } : { pausedPosition: null });
  }
});
