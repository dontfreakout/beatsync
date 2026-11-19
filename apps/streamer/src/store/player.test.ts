import { beforeEach, describe, expect, it } from "bun:test";

import { usePlayerStore, selectedUrl } from "./player";
import { initialRoomState, useRoomStore } from "./room";

const roomEvent = (event: Record<string, unknown>) => ({ type: "ROOM_EVENT", event });
const sources = (urls: string[], current?: string) =>
  roomEvent({ type: "SET_AUDIO_SOURCES", sources: urls.map((url) => ({ url })), currentAudioSource: current });
const action = (scheduledAction: Record<string, unknown>) => ({
  type: "SCHEDULED_ACTION",
  serverTimeToExecute: 5000,
  scheduledAction,
});

beforeEach(() => {
  useRoomStore.getState().reset();
  usePlayerStore.getState().reset();
  useRoomStore.getState().apply(sources(["a", "b"], "a"));
});

describe("local selection", () => {
  it("a_local_selection_that_left_the_queue_is_ignored", () => {
    usePlayerStore.getState().setLocalSelection("b");
    expect(selectedUrl(useRoomStore.getState(), usePlayerStore.getState().localSelection)).toBe("b");
    useRoomStore.getState().apply(sources(["a"]));
    expect(selectedUrl(useRoomStore.getState(), usePlayerStore.getState().localSelection)).toBe("a");
    expect(selectedUrl(initialRoomState, "x")).toBeNull();
  });

  it("a_new_play_clears_the_local_selection", () => {
    usePlayerStore.getState().setLocalSelection("b");
    useRoomStore.getState().apply(action({ type: "PLAY", audioSource: "a", trackTimeSeconds: 0 }));
    expect(usePlayerStore.getState().localSelection).toBeNull();
  });

  it("a_pause_keeps_the_local_selection", () => {
    usePlayerStore.getState().setLocalSelection("b");
    useRoomStore.getState().apply(action({ type: "PAUSE", audioSource: "a", trackTimeSeconds: 3 }));
    expect(usePlayerStore.getState().localSelection).toBe("b");
  });
});
