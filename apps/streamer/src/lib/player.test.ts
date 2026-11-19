import { describe, expect, it } from "bun:test";

import type { PlaybackState } from "@/store/room";

import {
  formatTime,
  formatTrackName,
  isPlaying,
  nextTrack,
  previousTrack,
  seekAction,
  isSelectedPlaying,
  heldSeekPosition,
  playbackKey,
  selectedPosition,
  sendHeldSeek,
  serverNowMs,
  skipAction,
  trackPosition,
} from "./player";

const play = (trackTimeSeconds: number, serverTimeToExecute = 10_000): PlaybackState => ({
  kind: "PLAY",
  serverTimeToExecute,
  audioSource: "a",
  trackTimeSeconds,
});
const pause = (trackTimeSeconds: number): PlaybackState => ({
  kind: "PAUSE",
  serverTimeToExecute: 10_000,
  audioSource: "a",
  trackTimeSeconds,
});

describe("serverNowMs", () => {
  it("adds_the_offset_and_is_null_without_one", () => {
    expect(serverNowMs(250, 1000)).toBe(1250);
    expect(serverNowMs(null, 1000)).toBeNull();
  });
});

describe("trackPosition", () => {
  it("position_follows_the_scheduled_play", () => {
    expect(trackPosition(play(5), 9_000, 100)).toBe(5); // before T: not backwards
    expect(trackPosition(play(5), 10_000, 100)).toBe(5);
    expect(trackPosition(play(5), 12_500, 100)).toBe(7.5);
    expect(trackPosition(play(5), 500_000, 100)).toBe(100); // clamped at the duration
    expect(trackPosition(play(5), 500_000, null)).toBe(490 + 5);
    expect(trackPosition(pause(42), 500_000, 100)).toBe(42);
    expect(trackPosition(pause(142), 500_000, 100)).toBe(100);
  });

  it("is_zero_without_playback_or_a_server_clock", () => {
    expect(trackPosition(null, 1000, 100)).toBe(0);
    expect(trackPosition(play(5), null, 100)).toBe(5);
    expect(trackPosition({ kind: "PAUSE", serverTimeToExecute: 1 }, 1000, 100)).toBe(0);
  });
});

describe("isPlaying", () => {
  it("plays_until_the_end_of_a_known_duration", () => {
    expect(isPlaying(play(0), 11_000, 100)).toBe(true);
    expect(isPlaying(play(0), 11_000, null)).toBe(true);
    expect(isPlaying(play(0), 200_000, 100)).toBe(false);
    expect(isPlaying(pause(3), 11_000, 100)).toBe(false);
    expect(isPlaying(null, 11_000, 100)).toBe(false);
  });
});

describe("skipping", () => {
  const urls = ["a", "b", "c"];

  it("next_and_previous_wrap_and_shuffle_never_repeats", () => {
    expect(nextTrack(urls, "a", false)).toBe("b");
    expect(nextTrack(urls, "c", false)).toBe("a");
    expect(previousTrack(urls, "a")).toBe("c");
    expect(previousTrack(urls, "b")).toBe("a");
    // The first random value hits the current index and is drawn again.
    const values = [0.1, 0.5];
    expect(nextTrack(urls, "a", true, () => values.shift() ?? 0.9)).toBe("b");
  });

  it("does_nothing_for_one_track_or_an_unknown_current", () => {
    expect(nextTrack(["a"], "a", false)).toBeNull();
    expect(nextTrack(urls, null, false)).toBeNull();
    expect(nextTrack(urls, "x", false)).toBeNull();
    expect(previousTrack(urls, "x")).toBeNull();
    expect(previousTrack([], "a")).toBeNull();
  });
});

describe("formatTime", () => {
  it("formats_like_the_web", () => {
    expect(formatTime(0)).toBe("00:00");
    expect(formatTime(Number.NaN)).toBe("00:00");
    expect(formatTime(-5)).toBe("00:00");
    expect(formatTime(65.9)).toBe("01:05");
    expect(formatTime(3725)).toBe("62:05");
  });
});

describe("formatTrackName", () => {
  const track = (over: Record<string, unknown>) =>
    ({
      title: " Song ",
      version: " Live ",
      performer: { name: "Ann" },
      album: { artists: [{ name: "Ann" }, { name: "Bob" }] },
      ...over,
    }) as never;

  it("joins_the_artists_title_and_version", () => {
    expect(formatTrackName(track({}))).toBe("Ann, Bob - Song (Live)");
    expect(formatTrackName(track({ version: null }))).toBe("Ann, Bob - Song");
    expect(formatTrackName(track({ performer: undefined, album: {}, title: "", version: undefined }))).toBe(
      "Unknown Artist - Unknown Title"
    );
  });
});

describe("selectedPosition", () => {
  const now = 12_000;
  it("follows a playing selected track", () => {
    expect(selectedPosition({ playback: play(5), selected: "a", pausedPosition: null, serverNow: now, durationS: 100 })).toBe(7);
  });
  it("keeps the paused position of the selected track", () => {
    expect(selectedPosition({ playback: pause(30), selected: "a", pausedPosition: null, serverNow: now, durationS: 100 })).toBe(30);
  });
  it("prefers a local seek while paused", () => {
    expect(selectedPosition({ playback: pause(30), selected: "a", pausedPosition: 12, serverNow: now, durationS: 100 })).toBe(12);
  });
  it("never shows the position of another track", () => {
    expect(selectedPosition({ playback: play(50), selected: "b", pausedPosition: null, serverNow: now, durationS: 100 })).toBe(0);
    expect(selectedPosition({ playback: pause(50), selected: "b", pausedPosition: 4, serverNow: now, durationS: 100 })).toBe(4);
  });
  it("is 0 without playback", () => {
    expect(selectedPosition({ playback: null, selected: "a", pausedPosition: null, serverNow: now, durationS: null })).toBe(0);
  });
});

describe("transport actions", () => {
  it("skip sends PLAY 0 only while playing", () => {
    expect(skipAction("b", true)).toEqual({ type: "PLAY", audioSource: "b", trackTimeSeconds: 0 });
    expect(skipAction("b", false)).toBeNull();
  });
  it("seek sends PLAY at the position only while playing", () => {
    expect(seekAction("a", true, 42)).toEqual({ type: "PLAY", audioSource: "a", trackTimeSeconds: 42 });
    expect(seekAction("a", false, 42)).toBeNull();
  });
});

describe("isSelectedPlaying", () => {
  const now = 1_000_000;
  const play = (audioSource: string): PlaybackState => ({ kind: "PLAY", audioSource, trackTimeSeconds: 0, serverTimeToExecute: now - 1000 });

  it("needs_a_play_of_the_selected_track", () => {
    expect(isSelectedPlaying(play("a"), "a", now, 100)).toBe(true);
    expect(isSelectedPlaying(play("a"), "b", now, 100)).toBe(false);
    expect(isSelectedPlaying(play("a"), null, now, 100)).toBe(false);
    expect(isSelectedPlaying(null, "a", now, 100)).toBe(false);
  });
});

describe("held seek", () => {
  const play = (serverTimeToExecute: number, trackTimeSeconds: number): PlaybackState => ({
    kind: "PLAY",
    audioSource: "a",
    serverTimeToExecute,
    trackTimeSeconds,
  });

  it("keys_a_playback_by_what_the_server_sets_not_by_the_running_position", () => {
    expect(playbackKey(play(1000, 5))).toBe(playbackKey(play(1000, 5)));
    expect(playbackKey(play(1000, 5))).not.toBe(playbackKey(play(2000, 60)));
    expect(playbackKey(null)).toBe("none");
  });

  it("keeps_the_committed_position_until_the_playback_changes", () => {
    const held = { position: 60, key: playbackKey(play(1000, 5)) };
    expect(heldSeekPosition(held, playbackKey(play(1000, 5)))).toBe(60);
    expect(heldSeekPosition(held, playbackKey(play(2000, 60)))).toBeNull();
    expect(heldSeekPosition(null, "none")).toBeNull();
  });

  type Hold = { position: number; key: string };
  /** A state like `useState`'s, for the hold. */
  const holdState = () => {
    const state = { held: null as Hold | null };
    const setHeld = (update: (current: Hold | null) => Hold | null) => {
      state.held = update(state.held);
    };
    return { state, setHeld };
  };

  it("holds_a_sent_seek", async () => {
    const { state, setHeld } = holdState();
    const hold = { position: 60, key: "k" };
    await sendHeldSeek(hold, setHeld, () => Promise.resolve(true));
    expect(state.held).toBe(hold);
  });

  it("releases_the_hold_of_a_seek_that_is_not_sent", async () => {
    // A closed socket: the seek bar must follow the playback again, not freeze.
    const { state, setHeld } = holdState();
    let sent: (sent: boolean) => void = () => {};
    const hold = { position: 60, key: "k" };
    const done = sendHeldSeek(hold, setHeld, () => new Promise((resolve) => (sent = resolve)));
    expect(state.held).toBe(hold);
    sent(false);
    await done;
    expect(state.held).toBeNull();
  });

  it("keeps_a_newer_hold_when_an_older_seek_is_not_sent", async () => {
    const { state, setHeld } = holdState();
    let sent: (sent: boolean) => void = () => {};
    const older = sendHeldSeek({ position: 60, key: "k" }, setHeld, () => new Promise((resolve) => (sent = resolve)));
    const newer = { position: 90, key: "k" };
    await sendHeldSeek(newer, setHeld, () => Promise.resolve(true));
    sent(false);
    await older;
    expect(state.held).toBe(newer);
  });
});
