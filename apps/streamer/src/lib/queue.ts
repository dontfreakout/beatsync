// Pure queue and search logic of the window, ported from the web client (`Queue.tsx`,
// `QueueSortableItem.tsx`, `SearchResults.tsx`).
import type { WSRequestType } from "@beatsync/shared";
import { arrayMove } from "@dnd-kit/sortable";

/** The new URL order after a drag, or null when nothing moves. */
export function reorderUrls(urls: string[], activeUrl: string, overUrl: string): string[] | null {
  const from = urls.indexOf(activeUrl);
  const to = urls.indexOf(overUrl);
  if (from < 0 || to < 0 || from === to) return null;
  return arrayMove(urls, from, to);
}

/** The items in `order` while it names exactly the same URLs; else the items as they are. */
export function orderByUrls<T extends { url: string }>(items: T[], order: string[] | null): T[] {
  if (order === null || order.length !== items.length) return items;
  const byUrl = new Map(items.map((item) => [item.url, item]));
  const ordered = order.map((url) => byUrl.get(url));
  return ordered.every((item) => item !== undefined) ? (ordered as T[]) : items;
}

/** Web `streamingTracksRef`: returns whether a request for `id` may go out; a repeat within `ttlMs` may not. */
export function createDuplicateGuard(ttlMs: number, now: () => number = Date.now): (id: number) => boolean {
  const last = new Map<number, number>();
  return (id) => {
    const at = now();
    const previous = last.get(id);
    if (previous !== undefined && at - previous < ttlMs) return false;
    last.set(id, at);
    return true;
  };
}

/**
 * What a click on a queue row sends (web `handleItemClick`): the selected track toggles
 * PAUSE/PLAY, another track starts at 0. A resume goes to the seek made while paused, if any.
 */
export function rowClickAction(args: {
  url: string;
  selected: string | null;
  playing: boolean;
  position: number;
  pausedPosition: number | null;
}): WSRequestType {
  const { url, selected, playing, position, pausedPosition } = args;
  if (url !== selected) return { type: "PLAY", audioSource: url, trackTimeSeconds: 0 };
  if (playing) return { type: "PAUSE", audioSource: url, trackTimeSeconds: position };
  return { type: "PLAY", audioSource: url, trackTimeSeconds: pausedPosition ?? position };
}
