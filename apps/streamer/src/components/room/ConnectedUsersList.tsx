import { Navigation, Users } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import { sendRoomAction } from "@/lib/actions";
import { isAdmin as isAdminClient, useRoomStore } from "@/store/room";
import { useSessionStore } from "@/store/session";
import { ConnectedUserItem } from "./ConnectedUserItem";

export function ConnectedUsersList() {
  const clientId = useSessionStore((s) => s.settings.clientId);
  const clients = useRoomStore((s) => s.clients);
  const currentUserIsAdmin = useRoomStore((s) => isAdminClient(s, clientId));

  const setAdmin = (targetId: string, isAdmin: boolean) =>
    sendRoomAction({ type: "SET_ADMIN", clientId: targetId, isAdmin });

  return (
    <TooltipProvider>
      <div>
        <div className="flex items-center justify-between px-4 pt-3">
          <div className="flex items-center gap-2 text-neutral-500">
            <Users className="h-3.5 w-3.5" />
            <h2 className="text-xs font-medium uppercase tracking-wider">Connected Users</h2>
          </div>
          <div className="flex items-center gap-2">
            <Tooltip delayDuration={300}>
              <TooltipTrigger asChild>
                <Navigation className="h-3 w-3 text-muted-foreground transition-colors hover:text-foreground" />
              </TooltipTrigger>
              <TooltipContent side="top" className="max-w-xs">
                <div className="p-2 font-mono text-xs">
                  Locations are estimated from IP addresses using public geolocation databases. Accuracy varies by
                  region.
                </div>
              </TooltipContent>
            </Tooltip>
            <Badge variant="outline">{clients.length}</Badge>
          </div>
        </div>

        <div className="px-4 pb-3">
          {clients.length === 0 ? (
            <div className="py-8 text-center text-xs text-neutral-500">No other users connected</div>
          ) : (
            <div className="relative mt-2.5">
              <div className="scrollbar-thin -mx-1 space-y-1 overflow-y-auto">
                {clients.map((client) => (
                  <ConnectedUserItem
                    key={client.clientId}
                    client={client}
                    isCurrentUser={client.clientId === clientId}
                    isAdmin={currentUserIsAdmin}
                    onSetAdmin={setAdmin}
                  />
                ))}
              </div>
            </div>
          )}
        </div>
      </div>
    </TooltipProvider>
  );
}
