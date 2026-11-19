import { useState } from "react";

import { type Held, nextHold, resolveHeld } from "@/lib/effects";

/**
 * A control value that follows the server, except: during a drag it shows the drag value, and after
 * the release it shows the committed value until the next server value arrives.
 */
export function useHeldValue<T>(server: T, equal: (a: T, b: T) => boolean = Object.is) {
  const [drag, setDrag] = useState<T | null>(null);
  const [held, setHeld] = useState<Held<T> | null>(null);
  // Adjust state during render: release the hold when the server value changes, so a later server
  // value equal to the old base is not hidden behind the stale held value.
  const released = nextHold(held, server, equal);
  if (released !== held) setHeld(released);
  const value = drag ?? resolveHeld(held, server, equal);
  return {
    value,
    isDragging: drag !== null,
    drag: (v: T) => setDrag(v),
    /** End a drag without a commit. */
    cancel: () => setDrag(null),
    commit: (v: T) => {
      setDrag(null);
      setHeld({ value: v, base: server });
    },
  };
}
