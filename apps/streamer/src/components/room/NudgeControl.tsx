import { ChevronsLeft, ChevronsRight } from "lucide-react";
import { motion } from "motion/react";
import { useState } from "react";
import { toast } from "sonner";

import { errorText, setNudge } from "@/api";
import { DEFAULT_NUDGE_STEP, type NudgeStep, nextNudgeStep, nudgeLabel } from "@/lib/nudge";
import { cn } from "@/lib/utils";
import { useSessionStore } from "@/store/session";

export function NudgeControl() {
  const nudgeMs = useSessionStore((s) => s.status.nudgeMs);
  const [step, setStep] = useState<NudgeStep>(DEFAULT_NUDGE_STEP);

  const nudge = (next: number) => {
    setNudge(next)
      .then((saved) => useSessionStore.getState().setSettings(saved))
      .catch((e) => toast.error(errorText(e)));
  };

  return (
    <motion.div
      className="flex flex-col items-start gap-1.5"
      initial={{ opacity: 0 }}
      animate={{ opacity: 1 }}
      transition={{ delay: 0.2 }}
    >
      <div className="flex items-center gap-2">
        <button
          className="rounded-sm p-1 text-neutral-400 transition-colors hover:bg-neutral-800 hover:text-white"
          onClick={() => nudge(nudgeMs + step)}
          title="Play earlier"
        >
          <ChevronsLeft className="h-3.5 w-3.5" />
        </button>
        <span
          className={cn(
            "min-w-[3.5rem] text-center font-mono text-[11px] tabular-nums",
            nudgeMs < 0 ? "text-green-500/70" : nudgeMs > 0 ? "text-red-500/70" : "text-neutral-400"
          )}
        >
          {nudgeLabel(nudgeMs)}
        </span>
        <button
          className="rounded-sm p-1 text-neutral-400 transition-colors hover:bg-neutral-800 hover:text-white"
          onClick={() => nudge(nudgeMs - step)}
          title="Play later"
        >
          <ChevronsRight className="h-3.5 w-3.5" />
        </button>
        <button
          className="flex size-6 cursor-pointer items-center justify-center rounded-full bg-neutral-800 font-mono text-[9px] text-neutral-500 transition-colors hover:bg-neutral-700 hover:text-neutral-300"
          onClick={() => setStep(nextNudgeStep(step))}
          title="Click to change step size"
        >
          {step}
        </button>
      </div>
    </motion.div>
  );
}
