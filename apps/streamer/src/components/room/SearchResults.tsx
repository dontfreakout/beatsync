import { type TrackType } from "@beatsync/shared";
import { Plus } from "lucide-react";
import { AnimatePresence, motion } from "motion/react";
import { useRef } from "react";
import { toast } from "sonner";

import { Button } from "@/components/ui/button";
import { sendRoomAction } from "@/lib/actions";
import { formatTrackName } from "@/lib/player";
import { createDuplicateGuard } from "@/lib/queue";
import { cn } from "@/lib/utils";
import { useSearchStore } from "@/store/search";
import { useSessionStore } from "@/store/session";

const DUPLICATE_ADD_MS = 3000;
const NO_COVER =
  "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='100' height='100' viewBox='0 0 100 100'%3E%3Crect width='100' height='100' fill='%23404040'/%3E%3Ctext x='50' y='50' text-anchor='middle' dy='.3em' fill='%23888' font-size='14'%3E♪%3C/text%3E%3C/svg%3E";

const formatDuration = (seconds: number) => `${Math.floor(seconds / 60)}:${(seconds % 60).toString().padStart(2, "0")}`;

function Spinner({ className, size }: { className: string; size: string }) {
  return (
    <div className={cn("relative flex items-center justify-center", size)}>
      <svg className="h-full w-full" viewBox="0 0 100 100">
        <motion.circle
          cx="50"
          cy="50"
          r="35"
          fill="none"
          stroke="currentColor"
          strokeWidth="8"
          strokeLinecap="round"
          className={className}
          strokeDasharray={2 * Math.PI * 35 * 0.25}
          animate={{ rotate: [0, 360] }}
          transition={{ duration: 1.5, repeat: Infinity, ease: "linear" }}
          style={{ transformOrigin: "center" }}
        />
      </svg>
    </div>
  );
}

function Message({ title, text, ring }: { title: string; text: string; ring?: "spin" | "still" }) {
  return (
    <motion.div initial={{ opacity: 0 }} animate={{ opacity: 1 }} className="flex flex-col items-center justify-center py-8">
      {ring && (
        <div className="relative mb-4 size-6">
          <svg className="h-full w-full" viewBox="0 0 100 100">
            <circle cx="50" cy="50" r="42" fill="none" stroke="currentColor" strokeWidth="6" className="text-neutral-800" />
            {ring === "spin" ? (
              <motion.circle
                cx="50"
                cy="50"
                r="42"
                fill="none"
                stroke="currentColor"
                strokeWidth="6"
                strokeLinecap="round"
                className="text-white"
                strokeDasharray={2 * Math.PI * 42}
                animate={{ strokeDashoffset: [2 * Math.PI * 42, 0] }}
                transition={{ duration: 2, repeat: Infinity, ease: "linear" }}
                style={{ transformOrigin: "center", transform: "rotate(-90deg)" }}
              />
            ) : (
              <circle
                cx="50"
                cy="50"
                r="42"
                fill="none"
                stroke="currentColor"
                strokeWidth="6"
                strokeLinecap="round"
                className="text-neutral-600"
                strokeDasharray={2 * Math.PI * 42}
                strokeDashoffset={2 * Math.PI * 42 * 0.75}
                style={{ transformOrigin: "center", transform: "rotate(-90deg)" }}
              />
            )}
          </svg>
        </div>
      )}
      <motion.h3
        className="mb-1 text-base font-medium tracking-tight text-white"
        initial={{ opacity: 0 }}
        animate={{ opacity: 1 }}
        transition={{ duration: 0.3, delay: 0.1 }}
      >
        {title}
      </motion.h3>
      <motion.p
        className="text-center text-xs text-neutral-400"
        initial={{ opacity: 0 }}
        animate={{ opacity: 1 }}
        transition={{ duration: 0.3, delay: 0.15 }}
      >
        {text}
      </motion.p>
    </motion.div>
  );
}

/** Port of the web `SearchResults`: the states and the rows of a music search. */
export function SearchResults({ className, onTrackSelect }: { className?: string; onTrackSelect?: () => void }) {
  const { query, items, total, searching, loadingMore, error, loadMore } = useSearchStore();
  const connected = useSessionStore((s) => s.status.connected);
  // The web `streamingTracksRef`: a repeat click on a track within 3 s is ignored.
  const allowAdd = useRef(createDuplicateGuard(DUPLICATE_ADD_MS));

  const handleAddTrack = async (track: TrackType) => {
    if (!connected) {
      toast.error("Not connected to server");
      return;
    }
    if (!allowAdd.current(track.id)) return;
    if (await sendRoomAction({ type: "STREAM_MUSIC", trackId: track.id, trackName: formatTrackName(track) })) {
      onTrackSelect?.();
    }
  };

  if (searching) return <Message ring="spin" title="Searching for music..." text="Finding tracks that match your query" />;

  if (error !== null) {
    return (
      <motion.div initial={{ opacity: 0 }} animate={{ opacity: 1 }} className="flex flex-col items-center justify-center py-8">
        <h3 className="mb-1 text-base font-medium tracking-tight">💀</h3>
        <p className="text-center text-xs text-neutral-400">{error}</p>
      </motion.div>
    );
  }

  if (items.length === 0) {
    return query ? (
      <Message ring="still" title="No results found" text="Try searching for a different artist, song, or album" />
    ) : (
      <Message title="Start typing to search for music..." text="Experimental. Free while in beta." />
    );
  }

  return (
    <motion.div initial={{ opacity: 0 }} animate={{ opacity: 1 }} className={className}>
      <AnimatePresence>
        <div className="space-y-1">
          {items.map((track, index) => (
            <motion.div
              key={track.id}
              initial={{ opacity: 0, filter: "blur(8px)" }}
              animate={{ opacity: 1, filter: "blur(0px)" }}
              exit={{ opacity: 0, filter: "blur(4px)" }}
              transition={{ duration: 0.3, delay: index * 0.06, ease: "easeInOut" }}
              className="group flex cursor-pointer items-center gap-3 rounded-md px-3 py-2 transition-all duration-200 hover:bg-neutral-800"
              onClick={() => void handleAddTrack(track)}
            >
              <div className="relative flex-shrink-0">
                <img
                  src={track.album.image.thumbnail || track.album.image.small}
                  alt={track.album.title}
                  className="h-10 w-10 rounded bg-neutral-800 object-cover"
                  onError={(e) => {
                    e.currentTarget.src = NO_COVER;
                  }}
                />
              </div>
              <div className="min-w-0 flex-1">
                <h4 className="truncate text-sm font-normal text-white">
                  {track.title}
                  {track.version && <span className="ml-1 text-neutral-500">({track.version})</span>}
                </h4>
                <p className="truncate text-xs text-neutral-400">{track.performer.name}</p>
              </div>
              <div className="text-xs text-neutral-500 transition-colors group-hover:text-neutral-400">
                {formatDuration(track.duration)}
              </div>
              <Button
                variant="ghost"
                size="sm"
                className="pointer-events-none h-6 w-6 p-0 opacity-0 transition-all duration-200 group-hover:opacity-100"
              >
                <Plus className="h-3 w-3" />
              </Button>
            </motion.div>
          ))}
        </div>
      </AnimatePresence>

      {items.length < total && (
        <motion.div initial={{ opacity: 0 }} animate={{ opacity: 1 }} transition={{ delay: 0.3 }} className="my-2 px-3">
          <Button
            variant="ghost"
            onMouseDown={(e) => {
              // Keep the focus in the input, so the blur handling of the container works.
              e.preventDefault();
            }}
            onClick={(e) => {
              e.stopPropagation();
              void loadMore();
            }}
            disabled={loadingMore}
            className="h-8 w-full cursor-pointer justify-center text-xs font-normal text-neutral-500 transition-all duration-200 hover:bg-neutral-800 hover:text-neutral-300 disabled:cursor-not-allowed disabled:opacity-50"
          >
            {loadingMore ? (
              <div className="flex items-center justify-center gap-2">
                <Spinner className="text-neutral-400" size="size-3" />
                <span>Loading more...</span>
              </div>
            ) : (
              "Show more results"
            )}
          </Button>
        </motion.div>
      )}
    </motion.div>
  );
}
