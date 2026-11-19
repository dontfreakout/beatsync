import { describe, expect, it } from "bun:test";

import { BEAT_INTERVAL_MS, beatIndex } from "@/lib/beats";

describe("beatIndex", () => {
  it("runs_at_55_bpm", () => {
    expect(BEAT_INTERVAL_MS).toBeCloseTo(1090.909, 3);
  });

  it("starts_beat_0_at_server_time_0", () => {
    expect(beatIndex(0)).toBe(0);
    expect(beatIndex(1090)).toBe(0);
  });

  it("steps_every_beat_interval", () => {
    expect(beatIndex(1091)).toBe(1);
    expect(beatIndex(2181)).toBe(1);
    expect(beatIndex(2182)).toBe(2);
    expect(beatIndex(60_001)).toBe(55);
  });

  it("rounds_like_the_web_at_an_exact_beat_time", () => {
    // 60000 ms is exactly beat 55, but the web's division gives 54.999…: the flash follows
    // the web devices, not the exact grid.
    expect(beatIndex(60_000)).toBe(54);
  });
});
