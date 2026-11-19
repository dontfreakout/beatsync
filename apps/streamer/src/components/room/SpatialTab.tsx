import { Info } from "lucide-react";

import { AudioEffects } from "@/components/room/AudioEffects";
import { UserGrid } from "@/components/room/UserGrid";
import { ScrollArea } from "@/components/ui/scroll-area";

/** Port of the web `right/SpatialAudio.tsx`. */
export function SpatialTab() {
  return (
    <ScrollArea className="h-full">
      <div className="w-full px-4">
        <UserGrid />
        <AudioEffects />
        <div className="mb-3 mt-1 flex flex-col gap-3 rounded-lg bg-neutral-800/30 px-4 py-3 text-neutral-400">
          <h5 className="mb-1 flex items-center gap-1.5 text-xs font-medium text-neutral-300">
            <Info className="h-3.5 w-3.5 flex-shrink-0 text-neutral-300" />
            What is this?
          </h5>
          <p className="text-xs leading-relaxed">
            This grid simulates a spatial audio environment. Drag the listening source around and hear how the volume
            changes on each device. Works best in person.
          </p>
        </div>
      </div>
    </ScrollArea>
  );
}
