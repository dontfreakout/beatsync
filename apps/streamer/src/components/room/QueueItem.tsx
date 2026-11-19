import { useSortable } from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";
import { AlertCircle, GripVertical, Loader2, MinusIcon, Pause, Play } from "lucide-react";
import { AnimatePresence, motion } from "motion/react";

import { sendRoomAction } from "@/lib/actions";
import { formatTime, serverNowMs, trackPosition } from "@/lib/player";
import { rowClickAction } from "@/lib/queue";
import { cn, extractFileNameFromUrl } from "@/lib/utils";
import { usePlayerStore } from "@/store/player";
import { useRoomStore } from "@/store/room";
import { useSessionStore } from "@/store/session";

const fade = { initial: { opacity: 0 }, animate: { opacity: 1, transition: { duration: 0.3, ease: "easeOut" as const } } };

interface QueueItemProps {
  url: string;
  index: number;
  /** The selected track: the local selection or the room's current track. */
  isSelected: boolean;
  isPlaying: boolean;
  /** Reorder and delete: the server accepts them from a user who can mutate, also during a live stream. */
  canMutate: boolean;
  /** Click to play or pause: the server refuses PLAY during a live stream. */
  canControl: boolean;
}

/** Port of the web `QueueSortableItem`. A URL with no track state is a normal row: the core loads only the current track. */
export function QueueItem({ url, index, isSelected, isPlaying, canMutate, canControl }: QueueItemProps) {
  const track = useSessionStore((s) => s.tracks[url]);
  const { attributes, listeners, setNodeRef, transform, transition, isDragging } = useSortable({ id: url });

  const isPlayingThis = isSelected && isPlaying;
  const isLoading = track?.state === "loading";
  const isError = track?.state === "error";

  const handleClick = () => {
    if (!canControl || isLoading || isError) return;
    const room = useRoomStore.getState();
    const { pausedPosition } = usePlayerStore.getState();
    const session = useSessionStore.getState();
    const state = session.tracks[url];
    const position =
      isSelected && room.playback?.audioSource === url
        ? trackPosition(room.playback, serverNowMs(session.status.offsetMs), state?.state === "ready" ? state.durationS : null)
        : 0;
    void sendRoomAction(rowClickAction({ url, selected: isSelected ? url : null, playing: isPlaying, position, pausedPosition }));
  };

  return (
    <div ref={setNodeRef} style={{ transform: CSS.Transform.toString(transform), transition }} {...attributes}>
      <motion.div
        layout
        initial={{ opacity: 0, y: 20 }}
        animate={{ opacity: 1, y: 0 }}
        exit={{ opacity: 0, y: -10, transition: { duration: 0.25, ease: "easeInOut" } }}
        transition={{
          layout: isDragging
            ? { duration: 0.15, ease: "backOut" }
            : { type: "spring", stiffness: 400, damping: 45, mass: 1 },
          opacity: { duration: 0.3, delay: Math.min(0.05 * index, 0.3), ease: "easeOut" },
          y: { duration: 0.3, delay: Math.min(0.05 * index, 0.3), ease: "easeOut" },
        }}
        className={cn(
          "group flex select-none items-center rounded-md py-3 pl-2 pr-4 transition-colors hover:bg-neutral-700/20",
          isSelected ? "text-white" : "text-neutral-300",
          !canMutate && "text-white/50",
          (isLoading || isError) && "cursor-not-allowed opacity-60",
          isDragging && "bg-neutral-700/20"
        )}
        onClick={handleClick}
      >
        {canMutate && (
          <motion.div
            {...listeners}
            className="p-1"
            initial={{ opacity: 0.25 }}
            animate={{ opacity: isDragging ? 1 : 0.25 }}
            whileHover={{ opacity: 1 }}
            style={{ cursor: "grab", touchAction: "none" }}
          >
            <GripVertical className="size-4" />
          </motion.div>
        )}

        <div className="relative flex h-6 w-6 flex-shrink-0 cursor-default select-none items-center justify-center">
          <AnimatePresence mode="wait">
            {isLoading ? (
              <motion.div
                key="loading"
                initial={{ opacity: 0 }}
                animate={{ opacity: 1 }}
                exit={{ opacity: 0, transition: { duration: 0.3, ease: "easeOut" } }}
                className="absolute inset-0 flex items-center justify-center"
              >
                <Loader2 className="size-4 animate-spin text-neutral-400" />
              </motion.div>
            ) : isError ? (
              <motion.div key="error" {...fade} className="absolute inset-0 flex items-center justify-center">
                <AlertCircle className="size-4 text-red-400" />
              </motion.div>
            ) : (
              <motion.div key="loaded" {...fade} className="absolute inset-0">
                <button className="absolute inset-0 flex h-full w-full select-none items-center justify-center text-sm text-white opacity-0 transition-transform hover:scale-110 group-hover:opacity-100">
                  {isPlayingThis ? <Pause className="size-3.5 fill-current stroke-1" /> : <Play className="size-3.5 fill-current" />}
                </button>
                <div className="flex h-full w-full select-none items-center justify-center group-hover:opacity-0">
                  {isPlayingThis ? (
                    <div className="flex h-4 w-4 items-end justify-center gap-[2px]">
                      <div className="h-[40%] w-[2px] animate-[sound-wave-1_1.2s_ease-in-out_infinite] bg-primary-500"></div>
                      <div className="h-[80%] w-[2px] animate-[sound-wave-2_1.4s_ease-in-out_infinite] bg-primary-500"></div>
                      <div className="h-[60%] w-[2px] animate-[sound-wave-3_1s_ease-in-out_infinite] bg-primary-500"></div>
                    </div>
                  ) : (
                    <span className={cn("select-none text-sm", isSelected ? "text-primary-400" : "text-neutral-400")}>
                      {index + 1}
                    </span>
                  )}
                </div>
              </motion.div>
            )}
          </AnimatePresence>
        </div>

        <div className="ml-3 min-w-0 flex-grow select-none">
          <div
            className={cn(
              "select-none truncate text-sm font-medium",
              isSelected && !isLoading && "text-primary-400",
              isError && "text-red-400",
              isLoading && "opacity-60"
            )}
          >
            {extractFileNameFromUrl(url)}
            {track?.state === "error" && <span className="ml-2 text-xs text-red-400">({track.message})</span>}
          </div>
        </div>

        <div className="ml-4 flex items-center gap-2">
          <div className="min-w-[3rem] select-none text-right text-xs text-neutral-500">
            {isSelected && track?.state === "ready" ? (
              <span>{formatTime(track.durationS)}</span>
            ) : (
              <span className="text-neutral-700">--:--</span>
            )}
          </div>

          {canMutate && (
            <button
              className="rounded-full p-1 text-neutral-500 transition-colors duration-150 hover:scale-110 hover:text-red-400 focus:scale-110 focus:text-red-400 focus:outline-none"
              onClick={(e) => {
                e.stopPropagation();
                void sendRoomAction({ type: "DELETE_AUDIO_SOURCES", urls: [url] });
              }}
            >
              <MinusIcon className="size-4" />
            </button>
          )}
        </div>
      </motion.div>
    </div>
  );
}
