import { describe, expect, it } from "bun:test";
import type { ChatMessageType } from "@beatsync/shared";

import { groupMessages } from "./chat";

const msg = (id: number, clientId: string, timestamp: number): ChatMessageType => ({
  id,
  clientId,
  username: clientId,
  text: `m${id}`,
  timestamp,
  isCreator: false,
});

describe("groupMessages", () => {
  it("joins_the_same_sender_less_than_the_window_apart", () => {
    const groups = groupMessages([msg(1, "a", 0), msg(2, "a", 59_000)]);
    expect(groups.map((g) => g.map((m) => m.id))).toEqual([[1, 2]]);
  });

  it("splits_the_same_sender_exactly_the_window_apart", () => {
    const groups = groupMessages([msg(1, "a", 0), msg(2, "a", 60_000)]);
    expect(groups.map((g) => g.map((m) => m.id))).toEqual([[1], [2]]);
  });

  it("splits_a_different_sender_one_second_apart", () => {
    const groups = groupMessages([msg(1, "a", 0), msg(2, "b", 1_000)]);
    expect(groups.map((g) => g.map((m) => m.id))).toEqual([[1], [2]]);
  });

  it("measures_the_window_from_the_last_message_of_the_group", () => {
    const groups = groupMessages([msg(1, "a", 0), msg(2, "a", 30_000), msg(3, "a", 60_000)]);
    expect(groups.map((g) => g.map((m) => m.id))).toEqual([[1, 2, 3]]);
  });

  it("returns_no_groups_for_no_messages", () => {
    expect(groupMessages([])).toEqual([]);
  });
});
