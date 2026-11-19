import {
  closestCenter,
  DndContext,
  type DragEndEvent,
  MouseSensor,
  TouchSensor,
  useSensor,
  useSensors,
} from "@dnd-kit/core";
import { restrictToVerticalAxis, restrictToWindowEdges } from "@dnd-kit/modifiers";
import { SortableContext, verticalListSortingStrategy } from "@dnd-kit/sortable";
import { AnimatePresence, motion } from "motion/react";
import { useEffect, useState } from "react";

import { LoadDefaultTracksButton } from "@/components/room/LoadDefaultTracksButton";
import { QueueItem } from "@/components/room/QueueItem";
import { sendRoomAction } from "@/lib/actions";
import { isSelectedPlaying, serverNowMs } from "@/lib/player";
import { orderByUrls, reorderUrls } from "@/lib/queue";
import { cn } from "@/lib/utils";
import { usePlayerStore, selectedUrl } from "@/store/player";
import { canControl as canControlIn, canMutate as canMutateIn, useRoomStore } from "@/store/room";
import { useSessionStore } from "@/store/session";

/** Re-renders every 500 ms while `active`, so a track that ends stops showing as playing. */
function useTick(active: boolean) {
  const [, setTick] = useState(0);
  useEffect(() => {
    if (!active) return;
    const id = setInterval(() => setTick((n) => n + 1), 500);
    return () => clearInterval(id);
  }, [active]);
}

/** Port of the web `Queue`: the sortable queue with play, pause and delete. */
export function Queue({ className }: { className?: string }) {
  const audioSources = useRoomStore((s) => s.audioSources);
  const currentAudioSource = useRoomStore((s) => s.currentAudioSource);
  const playback = useRoomStore((s) => s.playback);
  const clientId = useSessionStore((s) => s.settings.clientId);
  const canMutate = useRoomStore((s) => canMutateIn(s, clientId));
  const canControl = useRoomStore((s) => canControlIn(s, clientId));
  const localSelection = usePlayerStore((s) => s.localSelection);
  const offsetMs = useSessionStore((s) => s.status.offsetMs);
  const tracks = useSessionStore((s) => s.tracks);
  // The dragged order shows until the next SET_AUDIO_SOURCES.
  const [optimisticOrder, setOptimisticOrder] = useState<string[] | null>(null);
  useEffect(() => setOptimisticOrder(null), [audioSources]);

  const selected = selectedUrl({ audioSources, currentAudioSource }, localSelection);
  const selectedTrack = selected === null ? undefined : tracks[selected];
  const selectedDuration = selectedTrack?.state === "ready" ? selectedTrack.durationS : null;
  useTick(playback?.kind === "PLAY");
  const playing = isSelectedPlaying(playback, selected, serverNowMs(offsetMs), selectedDuration);

  const sensors = useSensors(
    useSensor(MouseSensor),
    useSensor(TouchSensor, { activationConstraint: { delay: 250, tolerance: 5 } })
  );

  const sources = orderByUrls(audioSources, optimisticOrder);

  const handleDragEnd = ({ active, over }: DragEndEvent) => {
    if (!canMutate || !over) return;
    const order = reorderUrls(
      sources.map((source) => source.url),
      String(active.id),
      String(over.id)
    );
    if (order === null) return;
    setOptimisticOrder(order);
    void sendRoomAction({ type: "REORDER_AUDIO_SOURCES", reorderedAudioSources: order.map((url) => ({ url })) }).then((sent) => {
      if (!sent) setOptimisticOrder(null);
    });
  };

  const items = (
    <AnimatePresence initial={true}>
      {sources.map((source, index) => (
        <QueueItem
          key={source.url}
          url={source.url}
          index={index}
          isSelected={selected === source.url}
          isPlaying={playing}
          canMutate={canMutate}
          canControl={canControl}
        />
      ))}
    </AnimatePresence>
  );

  return (
    <div className={cn("", className)}>
      <div className="space-y-1">
        {sources.length > 0 ? (
          canMutate ? (
            <DndContext
              sensors={sensors}
              onDragEnd={handleDragEnd}
              collisionDetection={closestCenter}
              modifiers={[restrictToVerticalAxis, restrictToWindowEdges]}
            >
              <SortableContext items={sources.map((source) => source.url)} strategy={verticalListSortingStrategy}>
                {items}
              </SortableContext>
            </DndContext>
          ) : (
            items
          )
        ) : (
          <motion.div
            initial={{ opacity: 0, scale: 0.98 }}
            animate={{ opacity: 1, scale: 1 }}
            transition={{ duration: 0.2, ease: "easeOut" }}
            className="flex w-full select-none flex-col items-center justify-center gap-3 text-center text-neutral-300"
          >
            {canMutate ? (
              <>
                <div className="text-sm text-neutral-400">No tracks yet</div>
                <LoadDefaultTracksButton />
              </>
            ) : (
              "No tracks available"
            )}
          </motion.div>
        )}
      </div>
    </div>
  );
}
