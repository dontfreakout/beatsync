import { describe, expect, it } from "bun:test";

import { createDuplicateGuard, orderByUrls, reorderUrls, rowClickAction } from "./queue";

describe("reorderUrls", () => {
  it("moves_the_dragged_url_to_the_target_slot", () => {
    expect(reorderUrls(["a", "b", "c"], "a", "c")).toEqual(["b", "c", "a"]);
    expect(reorderUrls(["a", "b", "c"], "c", "a")).toEqual(["c", "a", "b"]);
  });

  it("returns_null_when_nothing_moves", () => {
    expect(reorderUrls(["a", "b"], "a", "a")).toBeNull();
    expect(reorderUrls(["a", "b"], "a", "x")).toBeNull();
    expect(reorderUrls(["a", "b"], "x", "a")).toBeNull();
  });
});

describe("orderByUrls", () => {
  const items = [{ url: "a" }, { url: "b" }, { url: "c" }];

  it("applies_an_optimistic_order_of_the_same_set", () => {
    expect(orderByUrls(items, ["c", "a", "b"]).map((i) => i.url)).toEqual(["c", "a", "b"]);
  });

  it("ignores_an_order_for_another_set_or_none", () => {
    expect(orderByUrls(items, null)).toBe(items);
    expect(orderByUrls(items, ["a", "b"])).toBe(items);
    expect(orderByUrls(items, ["a", "b", "x"])).toBe(items);
  });
});

describe("createDuplicateGuard", () => {
  it("ignores_the_same_id_for_the_ttl", () => {
    let now = 1000;
    const allow = createDuplicateGuard(3000, () => now);
    expect(allow(7)).toBe(true);
    now = 3999;
    expect(allow(7)).toBe(false);
    expect(allow(8)).toBe(true);
    now = 4000;
    expect(allow(7)).toBe(true);
  });
});

describe("rowClickAction", () => {
  const base = { url: "u", selected: "u", playing: false, position: 12.5, pausedPosition: null };

  it("pauses_the_playing_selected_track_at_its_position", () => {
    expect(rowClickAction({ ...base, playing: true })).toEqual({
      type: "PAUSE",
      audioSource: "u",
      trackTimeSeconds: 12.5,
    });
  });

  it("resumes_the_selected_track_at_the_paused_seek_or_the_position", () => {
    expect(rowClickAction(base)).toEqual({ type: "PLAY", audioSource: "u", trackTimeSeconds: 12.5 });
    expect(rowClickAction({ ...base, pausedPosition: 40 })).toEqual({
      type: "PLAY",
      audioSource: "u",
      trackTimeSeconds: 40,
    });
  });

  it("plays_another_track_from_the_start", () => {
    expect(rowClickAction({ ...base, selected: "other", playing: true })).toEqual({
      type: "PLAY",
      audioSource: "u",
      trackTimeSeconds: 0,
    });
  });
});
