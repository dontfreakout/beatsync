import { useEffect, useRef } from "react";

import { beatIndex } from "@/lib/beats";
import { serverNowMs } from "@/lib/player";
import { useRoomStore } from "@/store/room";
import { useSessionStore } from "@/store/session";

const POLL_INTERVAL_MS = 10;

/**
 * Calls `onBeat` at each metronome beat. It uses the server clock without the nudge, and it runs
 * only while the room's metronome is on and the clock is synced. The beat that is running at the
 * start does not fire.
 */
export function useBeatTiming(onBeat: () => void) {
  const isMetronomeActive = useRoomStore((s) => s.metronome);
  const isSynced = useSessionStore((s) => s.status.synced);
  const onBeatRef = useRef(onBeat);
  useEffect(() => {
    onBeatRef.current = onBeat;
  });

  useEffect(() => {
    if (!isMetronomeActive || !isSynced) return;
    const current = () => {
      const now = serverNowMs(useSessionStore.getState().status.offsetMs);
      return now === null ? null : beatIndex(now);
    };
    let last = current();
    const interval = setInterval(() => {
      if (document.hidden) return;
      const index = current();
      if (index === null || index === last) return;
      last = index;
      onBeatRef.current();
    }, POLL_INTERVAL_MS);
    return () => clearInterval(interval);
  }, [isMetronomeActive, isSynced]);

  return { isMetronomeActive, isSynced };
}
