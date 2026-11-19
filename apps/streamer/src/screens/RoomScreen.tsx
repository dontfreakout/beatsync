import { motion } from "motion/react";
import { useEffect, useRef } from "react";
import { toast } from "sonner";

import { BeatFlash } from "@/components/room/BeatFlash";
import { Bottom } from "@/components/room/Bottom";
import { Left } from "@/components/room/Left";
import { InlineSearch } from "@/components/room/InlineSearch";
import { Queue } from "@/components/room/Queue";
import { Right } from "@/components/room/Right";
import { TopBar } from "@/components/room/TopBar";
import { useSessionStore } from "@/store/session";

const containerVariants = {
  hidden: { opacity: 0 },
  visible: { opacity: 1, transition: { duration: 0.5, staggerChildren: 0.1 } },
};

/**
 * Toasts each new status message (a stream failure, "Stopped by X", a lost server): the Stream
 * tab, which also shows it, may be closed.
 */
function useStatusMessageToast() {
  const message = useSessionStore((s) => s.status.message);
  // A message from before the room screen opened (e.g. a window reload) is not new.
  const shown = useRef(message);
  useEffect(() => {
    if (message !== null && message !== shown.current) toast(message);
    shown.current = message;
  }, [message]);
}

/** The three-column layout of the web dashboard's desktop branch; the search sits above the queue as in the web `Main`; */
export function RoomScreen() {
  useStatusMessageToast();
  return (
    <div className="flex h-screen w-full flex-col bg-neutral-950 text-white">
      <BeatFlash />
      <TopBar />
      <motion.div
        className="flex min-h-0 flex-1 flex-col overflow-hidden"
        variants={containerVariants}
        initial="hidden"
        animate="visible"
      >
        <div className="flex min-h-0 flex-1 overflow-hidden">
          <Left />
          <motion.div className="scrollbar-thin h-full flex-1 overflow-y-auto bg-neutral-950 bg-gradient-to-b from-neutral-900/90 to-neutral-950 backdrop-blur-xl">
            <motion.div className="p-6 pt-4">
              <div className="mb-6">
                <InlineSearch />
              </div>
              <Queue className="mb-8" />
            </motion.div>
          </motion.div>
          <Right />
        </div>
        <Bottom />
      </motion.div>
    </div>
  );
}
