import type { ExtractWSRequestFrom } from "@beatsync/shared";
import { requireRoom } from "@/websocket/middlewares";
import type { HandlerFunction } from "@/websocket/types";

export const handleUpdateLiveStream: HandlerFunction<ExtractWSRequestFrom["UPDATE_LIVE_STREAM"]> = ({
  ws,
  message,
}) => {
  const { room } = requireRoom(ws);

  if (room.getLiveStream()?.sourceClientId !== ws.data.clientId) {
    console.warn(`Live stream update ignored - ${ws.data.clientId} is not the source in room ${ws.data.roomId}`);
    return;
  }

  room.updateLiveStreamLabel(message.label);
};
