import { motion } from "motion/react";

import { Separator } from "@/components/ui/separator";
import { ConnectedUsersList } from "./ConnectedUsersList";
import { PlaybackPermissions } from "./PlaybackPermissions";
import { RoomHeader } from "./RoomHeader";

export function Left() {
  return (
    <motion.div className="scrollbar-thin flex w-80 flex-shrink-0 flex-col space-y-1 overflow-y-auto border-l border-neutral-800/50 bg-neutral-900/50 text-sm backdrop-blur-md">
      <RoomHeader />
      <Separator className="bg-neutral-800/50" />
      <PlaybackPermissions />
      <Separator className="bg-neutral-800/50" />
      <ConnectedUsersList />
      <motion.div className="mt-auto pb-4 pt-2 text-neutral-400">
        <div className="flex flex-col gap-2 border-t border-neutral-800/50 p-4">
          <h5 className="text-xs font-medium text-neutral-300">Tips</h5>
          <ul className="list-outside list-disc space-y-1.5 pl-4">
            <li className="text-xs leading-relaxed">{"Play on speaker directly. Don't use Bluetooth."}</li>
          </ul>
        </div>
      </motion.div>
    </motion.div>
  );
}
