import type { ClientDataType } from "@beatsync/shared";
import { ArrowUp, Crown, HeadphonesIcon, Rotate3D } from "lucide-react";
import { memo, useMemo, useRef } from "react";
import { throttle } from "throttle-debounce";

import { SpatialGainMeter } from "@/components/room/SpatialGainMeter";
import { Avatar, AvatarFallback } from "@/components/ui/avatar";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import { sendRoomAction } from "@/lib/actions";
import { canStartDrag, dragOutcome, pointerToGrid } from "@/lib/effects";
import { useHeldValue } from "@/lib/useHeldValue";
import { cn } from "@/lib/utils";
import { canMutate as canMutateIn, useRoomStore } from "@/store/room";
import { useSessionStore } from "@/store/session";

type Point = { x: number; y: number };

const samePoint = (a: Point, b: Point) => a.x === b.x && a.y === b.y;
const sendSource = (source: Point) => void sendRoomAction({ type: "SET_LISTENING_SOURCE", ...source });

const ClientAvatar = memo(function ClientAvatar({
  client,
  isCurrentUser,
  enabled,
}: {
  client: ClientDataType;
  isCurrentUser: boolean;
  enabled: boolean;
}) {
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <div
          className="absolute z-10 -translate-x-1/2 -translate-y-1/2 transition-all duration-100"
          style={{ left: `${client.position.x}%`, top: `${client.position.y}%`, opacity: enabled ? 1 : 0.5 }}
        >
          <div className="relative">
            <Avatar className="size-10 border-2 border-border">
              <AvatarFallback className={isCurrentUser ? "bg-primary-600" : "bg-neutral-600"}>
                {client.username.slice(0, 2).toUpperCase()}
              </AvatarFallback>
            </Avatar>
            {enabled && (
              <span
                className={cn(
                  "absolute inset-0 animate-ping rounded-full opacity-75",
                  isCurrentUser ? "bg-primary-400/40" : "bg-neutral-600"
                )}
              />
            )}
            {client.isAdmin && (
              <div className="absolute right-0 top-0 rounded-full bg-yellow-500 p-0.5">
                <Crown className="h-2.5 w-2.5 text-yellow-900" fill="currentColor" />
              </div>
            )}
          </div>
        </div>
      </TooltipTrigger>
      <TooltipContent side="top">
        <div className="text-xs font-medium">{client.username}</div>
        <div className="text-xs text-muted-foreground">
          {isCurrentUser ? "You" : "Connected"}
          {client.isAdmin && " • Admin"}
        </div>
      </TooltipContent>
    </Tooltip>
  );
});

/** Port of the web `UserGrid`: the switch, the client grid with the draggable listening source, the gain meter. */
export function UserGrid() {
  const clientId = useSessionStore((s) => s.settings.clientId);
  const canMutate = useRoomStore((s) => canMutateIn(s, clientId));
  const clients = useRoomStore((s) => s.clients);
  const enabled = useRoomStore((s) => s.spatial.enabled);
  const serverSource = useRoomStore((s) => s.spatial.listeningSource);
  const gridRef = useRef<HTMLDivElement>(null);
  const source = useHeldValue<Point>(serverSource, samePoint);
  const throttledSend = useMemo(() => throttle(100, sendSource), []);

  // The last point of the current drag; null until the pointer moves.
  const lastMoved = useRef<Point | null>(null);

  const moveSource = (e: React.PointerEvent) => {
    if (!source.isDragging || !gridRef.current) return;
    const point = pointerToGrid(e.clientX, e.clientY, gridRef.current.getBoundingClientRect());
    lastMoved.current = point;
    source.drag(point);
    throttledSend(point);
  };

  const endDrag = () => {
    if (!source.isDragging) return;
    const point = dragOutcome(lastMoved.current);
    lastMoved.current = null;
    // A late trailing throttled send must not overwrite the final position.
    throttledSend.cancel({ upcomingOnly: true });
    if (point === null) {
      source.cancel();
      return;
    }
    source.commit(point);
    sendSource(point);
  };

  return (
    <div className="mt-1 flex flex-col overflow-hidden">
      <div className="flex items-center justify-between pb-3">
        <div className="flex items-center gap-2 text-sm font-medium">
          <Rotate3D size={16} />
          <span>Spatial Audio</span>
        </div>
        <Switch
          checked={enabled}
          onCheckedChange={(checked) => {
            if (!canMutate) return;
            if (checked) sendSource(source.value);
            else void sendRoomAction({ type: "STOP_SPATIAL_AUDIO" });
          }}
          disabled={!canMutate}
          className={cn(!canMutate && "opacity-50")}
        />
      </div>

      <div className="flex min-h-0 flex-1 flex-col overflow-hidden">
        {clients.length === 0 ? (
          <div className="py-4 text-center text-muted-foreground">No other users connected</div>
        ) : (
          <>
            <div
              ref={gridRef}
              className={cn(
                "relative aspect-square w-full touch-none select-none overflow-hidden rounded-lg border border-border bg-[size:10%_10%] bg-[position:0_0] bg-[image:linear-gradient(to_right,rgba(55,65,81,0.1)_1px,transparent_1px),linear-gradient(to_bottom,rgba(55,65,81,0.1)_1px,transparent_1px)]",
                enabled ? "bg-muted/30" : "bg-muted/10 opacity-75"
              )}
            >
              <TooltipProvider>
                {clients.map((client) => (
                  <ClientAvatar
                    key={client.clientId}
                    client={client}
                    isCurrentUser={client.clientId === clientId}
                    enabled={enabled}
                  />
                ))}

                <Tooltip>
                  <TooltipTrigger asChild>
                    <div
                      className={cn("absolute z-40 -translate-x-1/2 -translate-y-1/2", canMutate && "cursor-move")}
                      style={{
                        left: `${source.value.x}%`,
                        top: `${source.value.y}%`,
                        opacity: enabled && canMutate ? 1 : 0.7,
                      }}
                      onPointerDown={(e) => {
                        if (!canStartDrag(canMutate, enabled)) return;
                        lastMoved.current = null;
                        e.currentTarget.setPointerCapture(e.pointerId);
                        source.drag(source.value);
                      }}
                      onPointerMove={moveSource}
                      onPointerUp={endDrag}
                      onPointerCancel={endDrag}
                    >
                      <div className="relative flex h-6 w-6 items-center justify-center rounded-full bg-primary-400/20 p-1">
                        <span className="relative flex h-3 w-3">
                          {enabled && (
                            <span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-primary-200 opacity-75" />
                          )}
                          <span className="relative inline-flex h-3 w-3 rounded-full bg-primary-500" />
                        </span>
                        <HeadphonesIcon className="absolute h-2 w-2 text-primary-100 opacity-80" />
                      </div>
                    </div>
                  </TooltipTrigger>
                  <TooltipContent side="top">
                    <div className="text-xs font-medium">Listening Source</div>
                    <div className="text-xs text-muted-foreground">Drag to reposition</div>
                  </TooltipContent>
                </Tooltip>
              </TooltipProvider>
            </div>

            <div className="mt-2.5">
              <SpatialGainMeter />
            </div>
          </>
        )}
        <div className="mb-4 mt-2 flex justify-end">
          <Button
            className="h-auto w-fit bg-neutral-700/60 px-3 py-1 text-xs text-white transition-colors duration-200 hover:bg-neutral-700 disabled:cursor-not-allowed disabled:opacity-50"
            size="sm"
            disabled={!canMutate}
            onClick={() => void sendRoomAction({ type: "REORDER_CLIENT", clientId })}
          >
            <ArrowUp className="size-4" /> Move to Top
          </Button>
        </div>
      </div>
    </div>
  );
}
