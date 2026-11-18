import type { ChatMessageType } from "@beatsync/shared";

/** The web chat's grouping window: the same sender within 1 minute shares a group. */
export const MESSAGE_GROUP_WINDOW_MS = 60_000;

/** Groups consecutive messages of one sender; a gap of `windowMs` or more starts a new group. */
export function groupMessages(messages: ChatMessageType[], windowMs = MESSAGE_GROUP_WINDOW_MS): ChatMessageType[][] {
  const groups: ChatMessageType[][] = [];
  for (const message of messages) {
    const group = groups[groups.length - 1];
    const last = group?.[group.length - 1];
    if (group && last && last.clientId === message.clientId && message.timestamp - last.timestamp < windowMs) {
      group.push(message);
    } else {
      groups.push([message]);
    }
  }
  return groups;
}

// Copied from the web client (apps/client/src/utils/time.ts).
const TIME_FORMAT: Intl.DateTimeFormatOptions = { hour: "numeric", minute: "2-digit", hour12: true };

/** "3:04 PM", "Yesterday 3:04 PM" or "Nov 5 at 3:04 PM". */
export function formatChatTimestamp(timestamp: number): string {
  const date = new Date(timestamp);
  const now = new Date();
  const yesterday = new Date(now);
  yesterday.setDate(yesterday.getDate() - 1);
  const time = date.toLocaleTimeString("en-US", TIME_FORMAT);
  if (date.toDateString() === now.toDateString()) return time;
  if (date.toDateString() === yesterday.toDateString()) return `Yesterday ${time}`;
  return `${date.toLocaleDateString("en-US", { month: "short", day: "numeric" })} at ${time}`;
}

/** The flag emoji of an ISO 3166-1 alpha-2 code, or "" for anything else. */
export function countryFlag(code: string): string {
  if (!/^[a-z]{2}$/i.test(code)) return "";
  return String.fromCodePoint(...[...code.toUpperCase()].map((c) => (c.codePointAt(0) ?? 0) + 127397));
}
