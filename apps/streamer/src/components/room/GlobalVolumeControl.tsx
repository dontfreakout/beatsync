import { Volume1, Volume2, VolumeX } from "lucide-react";
import { motion } from "motion/react";
import { useMemo } from "react";
import { throttle } from "throttle-debounce";

import { Slider } from "@/components/ui/slider";
import { sendRoomAction } from "@/lib/actions";
import { useHeldValue } from "@/lib/useHeldValue";
import { toggleMuteVolume, volumeIcon } from "@/lib/effects";
import { cn } from "@/lib/utils";
import { canMutate as canMutateIn, useRoomStore } from "@/store/room";
import { useSessionStore } from "@/store/session";

const sendVolume = (volume: number) => void sendRoomAction({ type: "SET_GLOBAL_VOLUME", volume });

/** Port of the web `GlobalVolumeControl` (desktop layout). The server's volume shows, except during a drag and after a commit until the server sends a new value. */
export function GlobalVolumeControl() {
  const clientId = useSessionStore((s) => s.settings.clientId);
  const canMutate = useRoomStore((s) => canMutateIn(s, clientId));
  const globalVolume = useRoomStore((s) => s.globalVolume);
  const held = useHeldValue(globalVolume);
  const throttledSend = useMemo(() => throttle(50, sendVolume), []);

  const volume = held.value;
  const icon = volumeIcon(volume);

  return (
    <motion.div className="flex items-center gap-2" initial={{ opacity: 0 }} animate={{ opacity: 1 }} transition={{ delay: 0.2 }}>
      <button
        className={cn("text-neutral-400 transition-colors", canMutate ? "hover:text-white" : "opacity-50")}
        disabled={!canMutate}
        onClick={() => {
          const next = toggleMuteVolume(volume);
          held.commit(next);
          sendVolume(next);
        }}
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
            held.drag(value / 100);
            throttledSend(value / 100);
          }}
          onValueCommit={([value]) => {
            if (!canMutate) return held.cancel();
            held.commit(value / 100);
            sendVolume(value / 100);
          }}
          disabled={!canMutate}
          className={cn("w-full", !canMutate && "opacity-50")}
        />
      </div>
    </motion.div>
  );
}
