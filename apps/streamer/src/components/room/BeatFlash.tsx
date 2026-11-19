import { motion } from "motion/react";
import { useCallback, useState } from "react";

import { useBeatTiming } from "@/hooks/useBeatTiming";

const HORIZONTAL_GRADIENT = "linear-gradient(90deg, transparent 0%, rgba(255,255,255,1) 50%, transparent 100%)";
const VERTICAL_GRADIENT = "linear-gradient(180deg, transparent 0%, rgba(255,255,255,1) 50%, transparent 100%)";
const EDGE_GLOW = "0 0 20px 4px rgba(255,255,255,0.4), 0 0 50px 10px rgba(255,255,255,0.15)";

const EDGES = [
  { edge: "top", className: "absolute top-0 left-0 right-0 h-[2px]", background: HORIZONTAL_GRADIENT, scaleAxis: "scaleX" },
  { edge: "bottom", className: "absolute bottom-0 left-0 right-0 h-[2px]", background: HORIZONTAL_GRADIENT, scaleAxis: "scaleX" },
  { edge: "left", className: "absolute top-0 bottom-0 left-0 w-[2px]", background: VERTICAL_GRADIENT, scaleAxis: "scaleY" },
  { edge: "right", className: "absolute top-0 bottom-0 right-0 w-[2px]", background: VERTICAL_GRADIENT, scaleAxis: "scaleY" },
] as const;

/** Port of the web `BeatFlash`: the screen edges flash on each metronome beat. */
export function BeatFlash() {
  const [beatKey, setBeatKey] = useState(0);
  const { isMetronomeActive, isSynced } = useBeatTiming(useCallback(() => setBeatKey((k) => k + 1), []));

  if (!isMetronomeActive || !isSynced) return null;

  return (
    <div className="pointer-events-none fixed inset-0 z-50">
      {EDGES.map(({ edge, className, background, scaleAxis }) => (
        <motion.div
          key={`${beatKey}-${edge}`}
          className={className}
          style={{ background, boxShadow: EDGE_GLOW }}
          initial={{ opacity: 1, [scaleAxis]: 0.3 }}
          animate={{ opacity: 0, [scaleAxis]: 1 }}
          transition={{ duration: 0.5, ease: "easeOut" }}
        />
      ))}
    </div>
  );
}
