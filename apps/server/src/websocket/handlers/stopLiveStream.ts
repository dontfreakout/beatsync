import type { ExtractWSRequestFrom } from "@beatsync/shared";
import { canClientMutate, requireRoom } from "@/websocket/middlewares";
import type { HandlerFunction } from "@/websocket/types";

export const handleStopLiveStream: HandlerFunction<ExtractWSRequestFrom["STOP_LIVE_STREAM"]> = ({ ws }) => {
  const { room } = requireRoom(ws);
  const { clientId } = ws.data;
  const stream = room.getLiveStream();
  if (!stream) return;

  if (stream.sourceClientId === clientId) {
    room.stopLiveStream();
  } else if (canClientMutate(room, clientId)) {
    room.stopLiveStream({ stoppedByClientId: clientId });
  } else {
    console.warn(`Live stream stop ignored - ${clientId} lacks permission in room ${ws.data.roomId}`);
  }
};
