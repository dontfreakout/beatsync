import { Volume1, Volume2, VolumeX } from "lucide-react";
import { motion } from "motion/react";
import { useMemo, useState } from "react";
import { throttle } from "throttle-debounce";

import { Slider } from "@/components/ui/slider";
import { sendRoomAction } from "@/lib/actions";
import { toggleMuteVolume, volumeIcon } from "@/lib/effects";
import { cn } from "@/lib/utils";
import { canMutate as canMutateIn, useRoomStore } from "@/store/room";
import { useSessionStore } from "@/store/session";

const sendVolume = (volume: number) => void sendRoomAction({ type: "SET_GLOBAL_VOLUME", volume });

/** Port of the web `GlobalVolumeControl` (desktop layout). The server's volume shows except during a drag. */
export function GlobalVolumeControl() {
  const clientId = useSessionStore((s) => s.settings.clientId);
  const canMutate = useRoomStore((s) => canMutateIn(s, clientId));
  const globalVolume = useRoomStore((s) => s.globalVolume);
  const [dragVolume, setDragVolume] = useState<number | null>(null);
  const throttledSend = useMemo(() => throttle(50, sendVolume), []);

  const volume = dragVolume ?? globalVolume;
  const icon = volumeIcon(volume);

  return (
    <motion.div className="flex items-center gap-2" initial={{ opacity: 0 }} animate={{ opacity: 1 }} transition={{ delay: 0.2 }}>
      <button
        className={cn("text-neutral-400 transition-colors", canMutate ? "hover:text-white" : "opacity-50")}
        disabled={!canMutate}
        onClick={() => sendVolume(toggleMuteVolume(volume))}
      >
        {icon === "x" ? <VolumeX className="h-4 w-4" /> : icon === "low" ? <Volume1 className="h-4 w-4" /> : <Volume2 className="h-4 w-4" />}
      </button>
      <div className="flex w-24 items-center">
        <Slider
          value={[volume * 100]}
          min={0}
          max={100}
          step={1}
          onValueChange={([value]) => {
            if (!canMutate) return;
            setDragVolume(value / 100);
            throttledSend(value / 100);
          }}
          onValueCommit={([value]) => {
            setDragVolume(null);
            if (canMutate) sendVolume(value / 100);
          }}
          disabled={!canMutate}
          className={cn("w-full", !canMutate && "opacity-50")}
        />
      </div>
    </motion.div>
  );
}
