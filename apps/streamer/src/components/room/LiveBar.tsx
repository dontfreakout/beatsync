import { Radio } from "lucide-react";
import { toast } from "sonner";

import { errorText, isStreaming, stopStream } from "@/api";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { sendRoomAction } from "@/lib/actions";
import { liveBarMode } from "@/lib/liveBar";
import { extractFileNameFromUrl } from "@/lib/utils";
import { canMutate as canMutateClient, useRoomStore } from "@/store/room";
import { useSessionStore } from "@/store/session";

export function LiveBar() {
  const clientId = useSessionStore((s) => s.settings.clientId);
  const streamingHere = useSessionStore((s) => isStreaming(s.status.phase));
  const stream = useRoomStore((s) => s.liveStream);
  const canMutate = useRoomStore((s) => canMutateClient(s, clientId));
  const currentSource = useRoomStore((s) => s.currentAudioSource);

  const mode = liveBarMode(streamingHere, stream, clientId);

  if (mode === "own-with-stop" || mode === "own") {
    return (
      <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-2 px-2 md:px-4">
        <Badge className="h-6 max-w-full gap-1.5 bg-red-600 text-red-50">
          <Radio className="size-3.5 shrink-0 animate-pulse" aria-hidden="true" />
          <span className="truncate">LIVE · You</span>
        </Badge>
        {mode === "own-with-stop" && (
          <Button
            variant="destructive"
            size="sm"
            onClick={() => stopStream().catch((e) => toast.error(errorText(e)))}
          >
            Stop
          </Button>
        )}
      </div>
    );
  }

  if (mode === "other" && stream) {
    const title = stream.label ? `LIVE · ${stream.sourceUsername} (${stream.label})` : `LIVE · ${stream.sourceUsername}`;
    return (
      <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-2 px-2 md:px-4">
        <Badge className="h-6 max-w-full gap-1.5 bg-red-600 text-red-50">
          <Radio className="size-3.5 shrink-0 animate-pulse" aria-hidden="true" />
          <span className="truncate">{title}</span>
        </Badge>
        {canMutate && (
          <Button variant="destructive" size="sm" onClick={() => sendRoomAction({ type: "STOP_LIVE_STREAM" })}>
            Stop stream
          </Button>
        )}
      </div>
    );
  }

  return (
    <div className="truncate px-2 text-center text-sm text-neutral-300 md:px-4">
      {currentSource ? extractFileNameFromUrl(currentSource) : "Nothing playing"}
    </div>
  );
}
