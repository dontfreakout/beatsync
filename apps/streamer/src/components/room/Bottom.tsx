import { motion } from "motion/react";

import { isStreaming } from "@/api";
import { liveBarMode } from "@/lib/liveBar";
import { useRoomStore } from "@/store/room";
import { useSessionStore } from "@/store/session";

import { GlobalVolumeControl } from "./GlobalVolumeControl";
import { LiveBar } from "./LiveBar";
import { NudgeControl } from "./NudgeControl";
import { Player } from "./Player";

export function Bottom() {
  const clientId = useSessionStore((s) => s.settings.clientId);
  const streamingHere = useSessionStore((s) => isStreaming(s.status.phase));
  const stream = useRoomStore((s) => s.liveStream);
  // As the web: the live bar replaces the player (and its Space shortcut) while a stream exists.
  const live = liveBarMode(streamingHere, stream, clientId) !== "none";

  return (
    <motion.div className="relative z-10 flex-shrink-0 border-t border-neutral-800/50 bg-neutral-900/10 p-4 shadow-[0_-5px_15px_rgba(0,0,0,0.1)] backdrop-blur-lg">
      <div className="mx-auto flex max-w-7xl items-center justify-between">
        <div className="absolute left-6">
          <NudgeControl />
        </div>
        <div className="mx-auto max-w-3xl flex-1">{live ? <LiveBar /> : <Player />}</div>
        <div className="absolute right-6">
          <GlobalVolumeControl />
        </div>
      </div>
    </motion.div>
  );
}
