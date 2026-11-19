import { Metronome as MetronomeIcon } from "lucide-react";

import { sendRoomAction } from "@/lib/actions";
import { cn } from "@/lib/utils";
import { canMutate as canMutateIn, useRoomStore } from "@/store/room";
import { useSessionStore } from "@/store/session";

/** Port of the web `MetronomeButton`, shown once the clock has synced. */
export function MetronomeButton() {
  const synced = useSessionStore((s) => s.status.syncedOnce);
  const clientId = useSessionStore((s) => s.settings.clientId);
  const active = useRoomStore((s) => s.metronome);
  const canMutate = useRoomStore((s) => canMutateIn(s, clientId));

  if (!synced) return null;

  return (
    <button
      className={cn(
        "flex cursor-pointer items-center gap-1 rounded px-2 py-0.5 font-mono text-[10px] transition-colors disabled:cursor-not-allowed disabled:opacity-50",
        active ? "bg-white text-black" : "bg-neutral-800 text-neutral-500 hover:bg-neutral-700 hover:text-neutral-300"
      )}
      disabled={!canMutate}
      onClick={() => void sendRoomAction({ type: "SET_METRONOME", enabled: !active })}
    >
      <MetronomeIcon className="h-3 w-3" />
      {active ? "stop" : "metronome"}
    </button>
  );
}
