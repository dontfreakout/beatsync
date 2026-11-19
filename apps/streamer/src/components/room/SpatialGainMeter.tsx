import { motion } from "motion/react";

import { gainColor } from "@/lib/effects";
import { useRoomStore } from "@/store/room";
import { useSessionStore } from "@/store/session";

/** The gain of this client in the spatial grid, as the web `SpatialGainMeter`. */
export function SpatialGainMeter() {
  const clientId = useSessionStore((s) => s.settings.clientId);
  const enabled = useRoomStore((s) => s.spatial.enabled);
  const gain = useRoomStore((s) => (s.spatial.enabled ? (s.spatial.gains[clientId] ?? 1) : 1));
  // The web limits the bar to 94% to keep the round end visible at full gain.
  const barWidthPercent = Math.min(94, Math.max(0, gain * 94));

  return (
    <div className="flex w-full items-center">
      <div className="pr-2.5 font-mono text-xs text-neutral-400">{Math.round(gain * 100)}%</div>
      <div className="relative flex h-3 w-full items-center overflow-hidden rounded-full bg-neutral-800/60 px-0.5">
        <motion.div
          className="absolute h-1.5 rounded-full"
          initial={{ width: 0 }}
          animate={{ width: `${barWidthPercent}%`, opacity: enabled ? 1 : 0.5, backgroundColor: gainColor(gain) }}
          transition={{ duration: 0.05, type: "tween", backgroundColor: { duration: 0.2 } }}
        />
      </div>
    </div>
  );
}
