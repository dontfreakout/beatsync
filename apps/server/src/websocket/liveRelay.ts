import { decodeLiveFrame, LIVE_STREAM_CONSTANTS } from "@beatsync/shared";
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

  // Every relayed frame is fanned out to the whole room
  if (data.byteLength > LIVE_STREAM_CONSTANTS.MAX_FRAME_BYTES) return false;

  const frame = decodeLiveFrame(data);
  if (frame?.header.streamId !== stream.streamId) return false;

  ws.publish(roomId, data);
  return true;
}
