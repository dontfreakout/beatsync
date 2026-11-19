import { AudioLines, Orbit, Sparkles } from "lucide-react";
import { motion } from "motion/react";
import { useMemo } from "react";
import { throttle } from "throttle-debounce";

import { Button } from "@/components/ui/button";
import { Slider } from "@/components/ui/slider";
import { sendRoomAction } from "@/lib/actions";
import { freqToSlider, lowPassLabel, sliderToFreq } from "@/lib/effects";
import { useHeldValue } from "@/lib/useHeldValue";
import { cn } from "@/lib/utils";
import { canMutate as canMutateIn, useRoomStore } from "@/store/room";
import { useSessionStore } from "@/store/session";

const sendFreq = (freq: number) => void sendRoomAction({ type: "SET_LOW_PASS_FREQ", freq });

function LowPassControl({ canMutate }: { canMutate: boolean }) {
  const lowPassFreq = useRoomStore((s) => s.lowPassFreq);
  const slider = useHeldValue(freqToSlider(lowPassFreq), (a, b) => Math.abs(a - b) < 1e-9);
  const throttledSend = useMemo(() => throttle(50, sendFreq), []);

  const freq = sliderToFreq(slider.value);
  const isActive = freq < 19000;

  return (
    <motion.div
      className={cn("rounded-md bg-neutral-800/20 p-3 transition-colors hover:bg-neutral-800/30", !canMutate && "opacity-50")}
    >
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-1.5 text-xs text-neutral-300">
          <AudioLines className={cn("h-3 w-3", isActive ? "text-primary-500" : "text-neutral-500")} />
          <span>Low-Pass</span>
        </div>
        <div className="min-w-[2.5rem] text-right text-xs text-neutral-500">{lowPassLabel(freq)}</div>
      </div>
      <div className="mt-2">
        <Slider
          value={[slider.value]}
          min={0}
          max={100}
          step={0.5}
          onValueChange={([value]) => {
            if (!canMutate) return;
            slider.drag(value);
            throttledSend(sliderToFreq(value));
          }}
          onValueCommit={([value]) => {
            if (!canMutate) return;
            // A late trailing throttled send must not overwrite the final value.
            throttledSend.cancel({ upcomingOnly: true });
            slider.commit(value);
            sendFreq(sliderToFreq(value));
          }}
          disabled={!canMutate}
          className="w-full"
        />
      </div>
    </motion.div>
  );
}

/** The "Audio Effects" card of the web Spatial tab: rotation Start/Stop and the low-pass filter. */
export function AudioEffects() {
  const clientId = useSessionStore((s) => s.settings.clientId);
  const canMutate = useRoomStore((s) => canMutateIn(s, clientId));

  return (
    <motion.div className="mt-1 space-y-2 py-3">
      <div className="flex items-center gap-2 text-sm font-medium">
        <Sparkles size={16} />
        <span>Audio Effects</span>
      </div>
      <div className="space-y-2">
        <motion.div
          className={cn("rounded-md bg-neutral-800/20 p-3 transition-colors hover:bg-neutral-800/30", !canMutate && "opacity-50")}
        >
          <div className="flex items-center justify-between">
            <div className="flex items-center gap-1.5 text-xs text-neutral-300">
              <Orbit className="h-3 w-3 text-primary-500" />
              <span>Rotation</span>
            </div>
            <div className="flex gap-2">
              <Button
                className="h-auto bg-primary-600/80 px-3 py-1 text-xs text-white hover:bg-primary-600"
                size="sm"
                disabled={!canMutate}
                onClick={() => void sendRoomAction({ type: "START_SPATIAL_AUDIO" })}
              >
                Start
              </Button>
              <Button
                className="h-auto bg-neutral-700/60 px-3 py-1 text-xs text-white hover:bg-neutral-700"
                size="sm"
                disabled={!canMutate}
                onClick={() => void sendRoomAction({ type: "STOP_SPATIAL_AUDIO" })}
              >
                Stop
              </Button>
            </div>
          </div>
        </motion.div>
        <LowPassControl canMutate={canMutate} />
      </div>
    </motion.div>
  );
}
