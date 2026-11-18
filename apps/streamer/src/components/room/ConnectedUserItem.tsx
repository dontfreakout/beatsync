import type { ClientDataType } from "@beatsync/shared";
import { Crown, Monitor, MoreVertical, User } from "lucide-react";
import { motion } from "motion/react";
import { memo } from "react";

import { Avatar, AvatarFallback, AvatarImage } from "@/components/ui/avatar";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuTrigger } from "@/components/ui/dropdown-menu";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";

export interface ConnectedUserItemProps {
  client: ClientDataType;
  isCurrentUser: boolean;
  /** Whether the own client is an admin. */
  isAdmin: boolean;
  onSetAdmin: (clientId: string, isAdmin: boolean) => void;
}

const LocationContent = ({ client }: { client: ClientDataType }) => (
  <div className="space-y-1.5">
    <div className="flex items-center gap-2">
      <div className="flex w-3 justify-center">
        {client.isAdmin ? (
          <Crown className="h-2.5 w-2.5 text-yellow-500" fill="currentColor" />
        ) : (
          <User className="h-3 w-3 text-muted-foreground" />
        )}
      </div>
      <p className="text-xs font-medium text-foreground">{client.username}</p>
    </div>
    {client.location ? (
      <div className="space-y-1 text-xs text-muted-foreground">
        <div className="flex items-center gap-2">
          <div className="flex w-3 justify-center">
            <span className="text-sm">{client.location.flagEmoji}</span>
          </div>
          <span className="text-foreground/70">
            {[[client.location.city, client.location.region].filter(Boolean).join(", "), client.location.country]
              .filter(Boolean)
              .join(" • ")}
          </span>
        </div>
      </div>
    ) : (
      <div className="flex items-center gap-2">
        <div className="w-3"></div>
        <p className="text-xs italic text-muted-foreground/60">No location data</p>
      </div>
    )}
  </div>
);

export const ConnectedUserItem = memo<ConnectedUserItemProps>(({ client, isCurrentUser, isAdmin, onSetAdmin }) => {
  const avatarContent = (
    <div className="relative">
      <Avatar className="h-8 w-8">
        <AvatarImage src={client.location?.flagSvgURL} className="h-full w-full object-cover" />
        <AvatarFallback className={isCurrentUser ? "bg-primary-600" : "bg-neutral-600"}>
          {client.username
            .split("-")
            .map((part) => part[0])
            .join("")
            .slice(0, 2)
            .toUpperCase()}
        </AvatarFallback>
      </Avatar>
      {client.isAdmin && (
        <div className="absolute -right-0.5 -top-1 rounded-full bg-yellow-500 p-0.5">
          <Crown className="h-2.5 w-2.5 text-yellow-900" fill="currentColor" />
        </div>
      )}
    </div>
  );

  return (
    <motion.div
      className={cn(
        "flex items-center gap-2 rounded-md p-1.5 text-sm transition-all duration-300",
        client.isCreator ? "bg-sky-500/10" : isCurrentUser ? "bg-primary-400/10" : "bg-transparent"
      )}
      initial={{ opacity: 0.8 }}
      animate={{ opacity: 1, scale: 1 }}
      transition={{ duration: 0.3 }}
    >
      <Tooltip delayDuration={100}>
        <TooltipTrigger asChild>{avatarContent}</TooltipTrigger>
        <TooltipContent
          side="top"
          align="center"
          collisionPadding={8}
          className="border-border/50 bg-background/95 px-3 py-2 font-mono backdrop-blur-sm"
        >
          <LocationContent client={client} />
        </TooltipContent>
      </Tooltip>
      <div className="flex min-w-0 flex-col">
        <div className="truncate text-xs font-medium">
          <span>{client.username}</span>
        </div>
      </div>
      <Badge
        variant={client.isCreator ? "default" : isCurrentUser ? "default" : "outline"}
        className={cn(
          "ml-auto h-5 min-w-[60px] shrink-0 py-0 text-center text-xs",
          client.isCreator ? "bg-sky-600 text-sky-50" : isCurrentUser ? "bg-primary-600 text-primary-50" : ""
        )}
      >
        {client.isCreator ? "Creator" : isCurrentUser ? "You" : "Connected"}
      </Badge>
      {client.kind === "streamer" && (
        <Badge variant="outline" className="h-5 shrink-0 border-red-500/60 py-0 text-xs text-red-400">
          Streamer
        </Badge>
      )}
      {client.kind === "app" && (
        <span className="shrink-0" title="Desktop app">
          <Monitor className="h-3 w-3 text-neutral-400" aria-label="Desktop app" />
        </span>
      )}
      {isAdmin && !isCurrentUser && (
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <Button variant="ghost" size="sm" className="h-6 w-6 p-0 hover:bg-neutral-700/50">
              <MoreVertical className="h-3 w-3" />
            </Button>
          </DropdownMenuTrigger>
          <DropdownMenuContent align="end" className="w-40">
            {client.isAdmin ? (
              <DropdownMenuItem onClick={() => onSetAdmin(client.clientId, false)} className="text-xs">
                <Crown className="mr-2 h-3 w-3 text-red-500" />
                Remove Admin
              </DropdownMenuItem>
            ) : (
              <DropdownMenuItem onClick={() => onSetAdmin(client.clientId, true)} className="text-xs">
                <Crown className="mr-2 h-3 w-3 text-green-500" />
                Make Admin
              </DropdownMenuItem>
            )}
          </DropdownMenuContent>
        </DropdownMenu>
      )}
    </motion.div>
  );
});

ConnectedUserItem.displayName = "ConnectedUserItem";
