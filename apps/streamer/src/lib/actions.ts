import type { WSRequestType } from "@beatsync/shared";
import { toast } from "sonner";

import { errorText, sendAction } from "@/api";

/**
 * Sends a room action and shows a failure (for example a closed socket) as a toast. Resolves
 * to whether the action was sent; never rejects.
 */
export function sendRoomAction(action: WSRequestType): Promise<boolean> {
  return sendAction(action).then(
    () => true,
    (e) => {
      toast.error(errorText(e));
      return false;
    }
  );
}
