import { decodeLiveFrame } from "@beatsync/shared";
import type { ServerWebSocket } from "bun";
import { globalManager } from "@/managers";
import type { WSData } from "@/utils/websocket";

/**
 * Forward a live stream binary frame from the source client to the rest of the room.
 * ws.publish excludes the sender. No logging: this runs for every frame.
 * Returns true when the frame was forwarded.
 */
export function relayLiveFrame(ws: ServerWebSocket<WSData>, data: Uint8Array): boolean {
  const { roomId, clientId } = ws.data;
  const room = globalManager.getRoom(roomId);
  if (!room) return false;

  room.markClientSeen(clientId);

  const stream = room.getLiveStream();
  if (stream?.sourceClientId !== clientId) return false;

  const frame = decodeLiveFrame(data);
  if (frame?.header.streamId !== stream.streamId) return false;

  ws.publish(roomId, data);
  return true;
}
