import { leaveRoom } from "@/api";
import { usePlayerStore } from "@/store/player";
import { useRoomStore } from "@/store/room";
import { useSearchStore } from "@/store/search";
import { useSessionStore } from "@/store/session";

/** Leaves the room, then clears the room state and returns to the join screen. */
export async function leaveAndReset(): Promise<void> {
  await leaveRoom();
  useRoomStore.getState().reset();
  usePlayerStore.getState().reset();
  useSearchStore.getState().reset();
  useSessionStore.getState().setTracks({});
  useSessionStore.getState().setJoined(false);
}
