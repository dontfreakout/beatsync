"use client";

import { useCanMutate, useGlobalStore } from "@/store/global";
import { useLiveStreamStore } from "@/store/liveStream";
import { sendWSRequest } from "@/utils/ws";
import { ClientActionEnum } from "@beatsync/shared";
import { Radio } from "lucide-react";
import { useEffect, useId } from "react";
import { toast } from "sonner";
import { Badge } from "../ui/badge";
import { Button } from "../ui/button";
import { Switch } from "../ui/switch";

export const LiveStreamBar = () => {
  const canMutate = useCanMutate();
  const stream = useLiveStreamStore((state) => state.stream);
  const playHere = useLiveStreamStore((state) => state.playHere);
  const setPlayHere = useLiveStreamStore((state) => state.setPlayHere);
  const status = useLiveStreamStore((state) => state.status);
  const isTooSlow = useLiveStreamStore((state) => state.isTooSlow);
  const switchId = useId();

  // One toast per rise of isTooSlow
  useEffect(() => {
    if (isTooSlow) toast.error("Connection too slow for the stream delay");
  }, [isTooSlow]);

  if (!stream) return null;

  const stopStream = () => {
    const socket = useGlobalStore.getState().socket;
    if (!socket) return;
    sendWSRequest({ ws: socket, request: { type: ClientActionEnum.enum.STOP_LIVE_STREAM } });
  };

  const title = stream.label ? `LIVE · ${stream.sourceUsername} (${stream.label})` : `LIVE · ${stream.sourceUsername}`;

  return (
    <div className="flex flex-col gap-2 px-2 md:px-4">
      <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-2">
        <Badge className="bg-red-600 text-red-50 max-w-full h-6 gap-1.5">
          <Radio className="size-3.5 shrink-0 animate-pulse" aria-hidden="true" />
          <span className="truncate">{title}</span>
        </Badge>
        <div className="flex items-center gap-3">
          <div className="flex items-center gap-2">
            <Switch id={switchId} checked={playHere} onCheckedChange={setPlayHere} />
            <label htmlFor={switchId} className="text-sm text-neutral-200 cursor-pointer">
              Play live stream here
            </label>
          </div>
          {canMutate && (
            <Button variant="destructive" size="sm" onClick={stopStream}>
              Stop stream
            </Button>
          )}
        </div>
      </div>
      {status === "waiting" && (
        <p className="text-xs text-neutral-400" role="status">
          Waiting for stream
        </p>
      )}
      <p className="text-xs text-neutral-500">
        On the laptop that streams, turn off &apos;Play live stream here&apos;.
      </p>
    </div>
  );
};
