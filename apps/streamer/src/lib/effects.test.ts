import { describe, expect, it } from "bun:test";

import {
  canStartDrag,
  dragOutcome,
  freqToSlider,
  gainColor,
  lowPassLabel,
  nextHold,
  pointerToGrid,
  resolveHeld,
  sliderToFreq,
  toggleMuteVolume,
  volumeIcon,
} from "./effects";

describe("low-pass slider", () => {
  it("maps_the_ends_of_the_log_scale", () => {
    expect(sliderToFreq(0)).toBe(20);
    expect(sliderToFreq(100)).toBe(20000);
    expect(freqToSlider(20)).toBeCloseTo(0, 6);
    expect(freqToSlider(20000)).toBeCloseTo(100, 6);
    expect(sliderToFreq(freqToSlider(1000))).toBe(1000);
  });

  it("labels_off_k_and_hz", () => {
    expect(lowPassLabel(20000)).toBe("OFF");
    expect(lowPassLabel(19000)).toBe("OFF");
    expect(lowPassLabel(18999)).toBe("19.0k");
    expect(lowPassLabel(1000)).toBe("1.0k");
    expect(lowPassLabel(999)).toBe("999");
    expect(lowPassLabel(20)).toBe("20");
  });
});

describe("gainColor", () => {
  it("follows_the_web_thresholds", () => {
    expect(gainColor(1)).toBe("#22c55e");
    expect(gainColor(0.9)).toBe("#22c55e");
    expect(gainColor(0.7)).toBe("#4ade80");
    expect(gainColor(0.5)).toBe("#86efac");
    expect(gainColor(0.3)).toBe("#a3a3a3");
    expect(gainColor(0.29)).toBe("#737373");
  });
});

describe("volumeIcon", () => {
  it("picks_x_low_or_high", () => {
    expect(volumeIcon(0)).toBe("x");
    expect(volumeIcon(0.3)).toBe("low");
    expect(volumeIcon(0.5)).toBe("high");
    expect(volumeIcon(1)).toBe("high");
  });
});

describe("toggleMuteVolume", () => {
  it("mutes a sound volume and restores 0.5 from mute", () => {
    expect(toggleMuteVolume(0.8)).toBe(0);
    expect(toggleMuteVolume(0)).toBe(0.5);
  });
});

describe("pointerToGrid", () => {
  const rect = { left: 10, top: 20, width: 200, height: 200 };

  it("rounds_the_pointer_to_the_0_100_grid", () => {
    expect(pointerToGrid(110, 120, rect)).toEqual({ x: 50, y: 50 });
    expect(pointerToGrid(10 + 67, 20 + 33.4, rect)).toEqual({ x: 34, y: 17 });
  });

  it("clamps_a_pointer_outside_the_grid", () => {
    expect(pointerToGrid(-500, 9999, rect)).toEqual({ x: 0, y: 100 });
    expect(pointerToGrid(9999, -500, rect)).toEqual({ x: 100, y: 0 });
  });
});

describe("resolveHeld", () => {
  const same = (a: number, b: number) => a === b;

  it("shows_the_server_value_without_a_held_value", () => {
    expect(resolveHeld(null, 5, same)).toBe(5);
  });

  it("keeps_the_committed_value_until_the_server_value_changes", () => {
    const held = { value: 9, base: 5 };
    expect(resolveHeld(held, 5, same)).toBe(9);
    expect(resolveHeld(held, 9, same)).toBe(9);
    expect(resolveHeld(held, 7, same)).toBe(7);
  });
});

describe("nextHold", () => {
  const same = (a: number, b: number) => a === b;

  it("releases_the_hold_so_a_later_server_value_equal_to_the_base_wins", () => {
    let held: { value: number; base: number } | null = { value: 9, base: 5 };
    held = nextHold(held, 5, same);
    expect(resolveHeld(held, 5, same)).toBe(9);
    held = nextHold(held, 9, same);
    expect(held).toBeNull();
    held = nextHold(held, 5, same);
    expect(resolveHeld(held, 5, same)).toBe(5);
  });
});

describe("held volume", () => {
  const same = (a: number, b: number) => a === b;

  it("keeps_the_committed_volume_until_the_server_sends_another", () => {
    const held = { value: 0.8, base: 0.3 };
    expect(resolveHeld(held, 0.3, same)).toBe(0.8);
    expect(resolveHeld(held, 0.5, same)).toBe(0.5);
    expect(nextHold(held, 0.5, same)).toBeNull();
  });
});

describe("listening source drag", () => {
  it("starts_no_drag_while_spatial_audio_is_off_or_the_user_cannot_mutate", () => {
    expect(canStartDrag(true, true)).toBe(true);
    expect(canStartDrag(true, false)).toBe(false);
    expect(canStartDrag(false, true)).toBe(false);
  });

  it("drops_a_drag_without_a_move", () => {
    expect(dragOutcome(null)).toBeNull();
  });

  it("commits_the_last_dragged_point", () => {
    expect(dragOutcome({ x: 3, y: 4 })).toEqual({ x: 3, y: 4 });
  });
});
