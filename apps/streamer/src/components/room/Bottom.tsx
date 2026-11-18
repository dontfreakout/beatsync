import { motion } from "motion/react";

import { LiveBar } from "./LiveBar";
import { NudgeControl } from "./NudgeControl";

export function Bottom() {
  return (
    <motion.div className="relative z-10 flex-shrink-0 border-t border-neutral-800/50 bg-neutral-900/10 p-4 shadow-[0_-5px_15px_rgba(0,0,0,0.1)] backdrop-blur-lg">
      <div className="mx-auto flex max-w-7xl items-center justify-between">
        <div className="absolute left-6">
          <NudgeControl />
        </div>
        <div className="mx-auto max-w-3xl flex-1">
          <LiveBar />
        </div>
      </div>
    </motion.div>
  );
}
