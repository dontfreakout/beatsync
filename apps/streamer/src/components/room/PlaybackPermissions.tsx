import { Crown, Play, Users } from "lucide-react";
import { motion } from "motion/react";

import { sendRoomAction } from "@/lib/actions";
import { cn } from "@/lib/utils";
import { isAdmin as isAdminClient, useRoomStore } from "@/store/room";
import { useSessionStore } from "@/store/session";

export function PlaybackPermissions() {
  const clientId = useSessionStore((s) => s.settings.clientId);
  const admin = useRoomStore((s) => isAdminClient(s, clientId));
  const isAdminOnly = useRoomStore((s) => s.permissions === "ADMIN_ONLY");

  const toggle = () =>
    sendRoomAction({ type: "SET_PLAYBACK_CONTROLS", permissions: isAdminOnly ? "EVERYONE" : "ADMIN_ONLY" });

  return (
    <div>
      <div className="flex items-center justify-between px-4 pt-3">
        <h2 className="flex items-center gap-2 text-xs font-medium uppercase tracking-wider text-neutral-500">
          <Play className="h-3.5 w-3.5" />
          <span>Playback Permissions</span>
        </h2>
      </div>

      <div className="px-4 pb-3">
        <button
          onClick={admin ? toggle : undefined}
          disabled={!admin}
          className={cn(
            "relative mt-2.5 flex h-8 w-full rounded-lg bg-neutral-800 p-0.5 focus:outline-none",
            admin ? "cursor-pointer" : "opacity-75"
          )}
        >
          <motion.div
            className={cn(
              "absolute inset-y-0.5 w-1/2 rounded-lg transition-colors duration-300",
              isAdminOnly ? "bg-yellow-600" : "bg-green-600"
            )}
            initial={false}
            animate={{ x: isAdminOnly ? "calc(100% - 4px)" : 0 }}
            transition={{ type: "spring", stiffness: 700, damping: 40 }}
          />
          <div
            className={cn(
              "relative z-10 flex h-full flex-1 items-center justify-center gap-1.5 text-xs font-medium transition-colors duration-200",
              !isAdminOnly ? "text-white" : "text-neutral-400"
            )}
          >
            <Users className="h-3 w-3" />
            <span>Everyone</span>
          </div>
          <div
            className={cn(
              "relative z-10 flex h-full flex-1 items-center justify-center gap-1.5 text-xs font-medium transition-colors duration-200",
              isAdminOnly ? "text-white" : "text-neutral-400"
            )}
          >
            <Crown className="h-3 w-3" />
            <span>Admins</span>
          </div>
        </button>
      </div>
    </div>
  );
}
