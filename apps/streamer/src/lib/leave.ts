import { leaveRoom } from "@/api";
import { useRoomStore } from "@/store/room";
import { useSessionStore } from "@/store/session";

/** Leaves the room, then clears the room state and returns to the join screen. */
export async function leaveAndReset(): Promise<void> {
  await leaveRoom();
  useRoomStore.getState().reset();
  useSessionStore.getState().setJoined(false);
}
