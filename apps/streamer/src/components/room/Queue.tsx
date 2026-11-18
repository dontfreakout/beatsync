import { AnimatePresence, motion } from "motion/react";

import { cn, extractFileNameFromUrl } from "@/lib/utils";
import { useRoomStore } from "@/store/room";

/** The room's queue, read-only. Queue changes come with part 2. */
export function Queue({ className }: { className?: string }) {
  const audioSources = useRoomStore((s) => s.audioSources);
  const currentAudioSource = useRoomStore((s) => s.currentAudioSource);
  const isPlaying = useRoomStore((s) => s.playback?.kind === "PLAY");

  return (
    <div className={className}>
      <div className="space-y-1">
        {audioSources.length > 0 ? (
          <AnimatePresence initial={true}>
            {audioSources.map((source, index) => {
              const isSelected = currentAudioSource === source.url;
              const isPlayingThis = isSelected && isPlaying;
              return (
                <motion.div
                  key={source.url}
                  layout
                  initial={{ opacity: 0, y: 20 }}
                  animate={{ opacity: 1, y: 0 }}
                  exit={{ opacity: 0, y: -10, transition: { duration: 0.25, ease: "easeInOut" } }}
                  transition={{
                    layout: { type: "spring", stiffness: 400, damping: 45, mass: 1 },
                    opacity: { duration: 0.3, delay: Math.min(0.05 * index, 0.3), ease: "easeOut" },
                    y: { duration: 0.3, delay: Math.min(0.05 * index, 0.3), ease: "easeOut" },
                  }}
                  className={cn(
                    "flex select-none items-center rounded-md py-3 pl-2 pr-4 transition-colors",
                    isSelected ? "text-white" : "text-neutral-300"
                  )}
                >
                  <div className="flex h-6 w-6 flex-shrink-0 select-none items-center justify-center">
                    {isPlayingThis ? (
                      <div className="flex h-4 w-4 items-end justify-center gap-[2px]">
                        <div className="h-[40%] w-[2px] animate-[sound-wave-1_1.2s_ease-in-out_infinite] bg-primary-500"></div>
                        <div className="h-[80%] w-[2px] animate-[sound-wave-2_1.4s_ease-in-out_infinite] bg-primary-500"></div>
                        <div className="h-[60%] w-[2px] animate-[sound-wave-3_1s_ease-in-out_infinite] bg-primary-500"></div>
                      </div>
                    ) : (
                      <span className={cn("text-sm", isSelected ? "text-primary-400" : "text-neutral-400")}>
                        {index + 1}
                      </span>
                    )}
                  </div>
                  <div className="ml-3 min-w-0 flex-grow select-none">
                    <div className={cn("truncate text-sm font-medium", isSelected && "text-primary-400")}>
                      {extractFileNameFromUrl(source.url)}
                    </div>
                  </div>
                </motion.div>
              );
            })}
          </AnimatePresence>
        ) : (
          <motion.div
            initial={{ opacity: 0, scale: 0.98 }}
            animate={{ opacity: 1, scale: 1 }}
            transition={{ duration: 0.2, ease: "easeOut" }}
            className="flex w-full select-none flex-col items-center justify-center gap-3 text-center text-neutral-300"
          >
            No tracks available
          </motion.div>
        )}
      </div>
    </div>
  );
}
