import type { ExtractWSRequestFrom } from "@beatsync/shared";
import { sendUnicast } from "@/utils/responses";
import { canClientMutate, requireRoom } from "@/websocket/middlewares";
import type { HandlerFunction } from "@/websocket/types";

export const handleStartLiveStream: HandlerFunction<ExtractWSRequestFrom["START_LIVE_STREAM"]> = ({
  ws,
  message,
  server,
}) => {
  const { room } = requireRoom(ws);
  const { clientId } = ws.data;

  if (!canClientMutate(room, clientId)) {
    sendUnicast({ ws, message: { type: "LIVE_STREAM_REJECTED", reason: "NOT_ALLOWED" } });
    return;
  }

  const active = room.getLiveStream();
  if (active && active.sourceClientId !== clientId) {
    sendUnicast({ ws, message: { type: "LIVE_STREAM_REJECTED", reason: "ALREADY_ACTIVE" } });
    return;
  }

  room.startLiveStream({ sourceClientId: clientId, request: message, server });
};
