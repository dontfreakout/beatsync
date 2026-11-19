import { Crown, Hash, LogOut, Users } from "lucide-react";
import { motion } from "motion/react";
import { useState } from "react";
import { toast } from "sonner";

import { MAX_SYNC_MEASUREMENTS, errorText } from "@/api";
import { Button } from "@/components/ui/button";
import { leaveAndReset } from "@/lib/leave";
import { isAdmin as isAdminClient, useRoomStore } from "@/store/room";
import { useSessionStore } from "@/store/session";

const RING_LENGTH = 31.4;

const formatMs = (value: number | null) => (value === null ? "-" : `${value.toFixed(2)}ms`);

export function TopBar() {
  const settings = useSessionStore((s) => s.settings);
  const status = useSessionStore((s) => s.status);
  const userCount = useRoomStore((s) => s.clients.length);
  const admin = useRoomStore((s) => isAdminClient(s, settings.clientId));
  const count = status.syncMeasurements;
  // Leave stops a running stream first, which can take about 2 s.
  const [leaving, setLeaving] = useState(false);

  const leave = () => {
    setLeaving(true);
    leaveAndReset()
      .catch((e) => toast.error(errorText(e)))
      .finally(() => setLeaving(false));
  };

  return (
    <div className="z-50 flex h-8 items-center justify-between border-b border-zinc-800 bg-black/80 px-4 backdrop-blur-md">
      <div className="flex items-center space-x-4 py-2 text-xs text-neutral-400 md:py-0">
        {admin && (
          <div className="flex items-center">
            <Crown className="h-3 w-3 text-green-500" fill="currentColor" />
          </div>
        )}
        <span className="font-medium">Beatsync</span>

        <div className="flex items-center">
          <motion.svg width="14" height="14" viewBox="0 0 14 14" className="mr-1">
            <circle
              cx="7"
              cy="7"
              r="5"
              fill="none"
              stroke="currentColor"
              strokeWidth="1.5"
              className="text-neutral-600"
            />
            <motion.circle
              cx="7"
              cy="7"
              r="5"
              fill="none"
              stroke="currentColor"
              strokeWidth="1.5"
              className="text-green-500"
              strokeLinecap="round"
              transform="rotate(-90 7 7)"
              initial={{ strokeDasharray: `0 ${RING_LENGTH}` }}
              animate={{ strokeDasharray: `${(count / MAX_SYNC_MEASUREMENTS) * RING_LENGTH} ${RING_LENGTH}` }}
              transition={{ duration: 0.3, ease: "easeInOut" }}
            />
          </motion.svg>
          <span className="text-xs">
            {count}/{MAX_SYNC_MEASUREMENTS}
          </span>
        </div>
        <div className="flex items-center">
          <Hash size={12} className="mr-1" />
          <span>{settings.roomId}</span>
        </div>
        <div className="flex items-center">
          <Users size={12} className="mr-1" />
          <span className="mr-1.5">
            {userCount} {userCount === 1 ? "user" : "users"}
          </span>
        </div>
        <div>|</div>
        <div className="flex items-center space-x-2">
          <span>Offset: {formatMs(status.offsetMs)}</span>
          <span>RTT: {formatMs(status.rttMs)}</span>
        </div>
      </div>

      <Button
        variant="ghost"
        size="sm"
        className="h-6 gap-1 px-2 text-xs text-neutral-400 hover:text-white"
        onClick={leave}
        disabled={leaving}
      >
        <LogOut className="size-3" />
        {leaving ? "Leaving…" : "Leave"}
      </Button>
    </div>
  );
}
