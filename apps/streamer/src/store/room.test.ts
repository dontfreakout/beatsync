import { describe, expect, it } from "bun:test";

import { applyServerMessage, canMutate, initialRoomState, isAdmin, type RoomState } from "./room";

const client = (clientId: string, isAdmin = false) => ({
  username: clientId,
  clientId,
  rtt: 0,
  compensationMs: 0,
  nudgeMs: 0,
  position: { x: 50, y: 50 },
  lastNtpResponse: 0,
  lastSeenAt: 0,
  lastLivenessPingAt: 0,
  isAdmin,
  isCreator: false,
  kind: "web" as const,
  joinedAt: 1,
});

const roomEvent = (event: Record<string, unknown>) => ({ type: "ROOM_EVENT", event });
const chatMessage = (id: number) => ({
  id,
  clientId: "a",
  username: "a",
  text: `message ${id}`,
  timestamp: id,
  isCreator: false,
});
const chatUpdate = (ids: number[], isFullSync: boolean, newestId: number) =>
  roomEvent({ type: "CHAT_UPDATE", messages: ids.map(chatMessage), isFullSync, newestId });
const stream = {
  streamId: 3,
  sourceClientId: "app-1",
  sourceUsername: "pc",
  label: "Firefox",
  codec: "opus",
  sampleRate: 48000,
  channels: 2,
  frameDurationMs: 20,
  delayMs: 1000,
  startedAt: 5,
};

const apply = (state: RoomState, ...messages: unknown[]) => messages.reduce<RoomState>(applyServerMessage, state);
const chatIds = (state: RoomState) => state.chat.messages.map((m) => m.id);

describe("applyServerMessage", () => {
  it("client_change_replaces_the_client_list", () => {
    const first = apply(initialRoomState, roomEvent({ type: "CLIENT_CHANGE", clients: [client("a"), client("b")] }));
    expect(first.clients.map((c) => c.clientId)).toEqual(["a", "b"]);
    const second = apply(first, roomEvent({ type: "CLIENT_CHANGE", clients: [client("b")] }));
    expect(second.clients.map((c) => c.clientId)).toEqual(["b"]);
  });

  it("set_audio_sources_sets_the_queue_and_current_source", () => {
    const state = apply(
      initialRoomState,
      roomEvent({ type: "SET_AUDIO_SOURCES", sources: [{ url: "u1" }, { url: "u2" }], currentAudioSource: "u2" })
    );
    expect(state.audioSources).toEqual([{ url: "u1" }, { url: "u2" }]);
    expect(state.currentAudioSource).toBe("u2");
    const cleared = apply(state, roomEvent({ type: "SET_AUDIO_SOURCES", sources: [] }));
    expect(cleared.audioSources).toEqual([]);
    expect(cleared.currentAudioSource).toBeNull();
  });

  it("set_audio_sources_without_a_current_source_keeps_it_while_it_is_in_the_queue", () => {
    const playing = apply(
      initialRoomState,
      roomEvent({ type: "SET_AUDIO_SOURCES", sources: [{ url: "u1" }, { url: "u2" }], currentAudioSource: "u2" })
    );
    const reordered = apply(playing, roomEvent({ type: "SET_AUDIO_SOURCES", sources: [{ url: "u2" }, { url: "u1" }] }));
    expect(reordered.currentAudioSource).toBe("u2");
  });

  it("set_audio_sources_that_removes_the_current_source_clears_it", () => {
    const playing = apply(
      initialRoomState,
      roomEvent({ type: "SET_AUDIO_SOURCES", sources: [{ url: "u1" }, { url: "u2" }], currentAudioSource: "u2" })
    );
    const deleted = apply(playing, roomEvent({ type: "SET_AUDIO_SOURCES", sources: [{ url: "u1" }] }));
    expect(deleted.currentAudioSource).toBeNull();
  });

  it("scheduled_play_sets_the_current_source_and_pause_keeps_it", () => {
    const played = apply(initialRoomState, {
      type: "SCHEDULED_ACTION",
      serverTimeToExecute: 1,
      scheduledAction: { type: "PLAY", audioSource: "u9", trackTimeSeconds: 0 },
    });
    expect(played.currentAudioSource).toBe("u9");
    const paused = apply(played, {
      type: "SCHEDULED_ACTION",
      serverTimeToExecute: 2,
      scheduledAction: { type: "PAUSE", audioSource: "u8", trackTimeSeconds: 1 },
    });
    expect(paused.currentAudioSource).toBe("u9");
  });

  it("chat_full_sync_replaces_and_an_update_appends_only_newer_ids", () => {
    let state = apply(initialRoomState, chatUpdate([1, 2], true, 2));
    expect(chatIds(state)).toEqual([1, 2]);
    state = apply(state, chatUpdate([2, 3], false, 3));
    expect(chatIds(state)).toEqual([1, 2, 3]);
    expect(state.chat.newestId).toBe(3);
    state = apply(state, chatUpdate([7], true, 7));
    expect(chatIds(state)).toEqual([7]);
    expect(state.chat.newestId).toBe(7);
  });

  it("live_stream_state_sets_and_clears_the_stream", () => {
    const set = apply(initialRoomState, roomEvent({ type: "LIVE_STREAM_STATE", stream }));
    expect(set.liveStream?.streamId).toBe(3);
    const cleared = apply(set, roomEvent({ type: "LIVE_STREAM_STATE", stream: null, stoppedByClientId: "x" }));
    expect(cleared.liveStream).toBeNull();
  });

  it("scheduled_play_and_pause_set_the_playback_state", () => {
    const played = apply(initialRoomState, {
      type: "SCHEDULED_ACTION",
      serverTimeToExecute: 1000,
      scheduledAction: { type: "PLAY", audioSource: "u1", trackTimeSeconds: 4.5 },
    });
    expect(played.playback).toEqual({
      kind: "PLAY",
      serverTimeToExecute: 1000,
      audioSource: "u1",
      trackTimeSeconds: 4.5,
    });
    const paused = apply(played, {
      type: "SCHEDULED_ACTION",
      serverTimeToExecute: 2000,
      scheduledAction: { type: "PAUSE", audioSource: "u1", trackTimeSeconds: 9 },
    });
    expect(paused.playback?.kind).toBe("PAUSE");
    expect(paused.playback?.serverTimeToExecute).toBe(2000);
    expect(paused.playback?.trackTimeSeconds).toBe(9);
  });

  it("snapshot_messages_rebuild_the_room", () => {
    const state = apply(
      initialRoomState,
      roomEvent({ type: "CLIENT_CHANGE", clients: [client("a", true)] }),
      roomEvent({ type: "SET_PLAYBACK_CONTROLS", permissions: "EVERYONE" }),
      roomEvent({ type: "SET_AUDIO_SOURCES", sources: [{ url: "u1" }], currentAudioSource: "u1" }),
      roomEvent({ type: "LIVE_STREAM_STATE", stream }),
      chatUpdate([1, 2], true, 2),
      { type: "SCHEDULED_ACTION", serverTimeToExecute: 1, scheduledAction: { type: "GLOBAL_VOLUME_CONFIG", volume: 0.5, rampTime: 0 } },
      {
        type: "SCHEDULED_ACTION",
        serverTimeToExecute: 9,
        scheduledAction: { type: "PLAY", audioSource: "u1", trackTimeSeconds: 0 },
      },
      { type: "STREAM_JOB_UPDATE", activeJobCount: 0 }
    );
    expect(state.clients).toHaveLength(1);
    expect(state.permissions).toBe("EVERYONE");
    expect(state.audioSources).toEqual([{ url: "u1" }]);
    expect(state.currentAudioSource).toBe("u1");
    expect(state.liveStream?.label).toBe("Firefox");
    expect(chatIds(state)).toEqual([1, 2]);
    expect(state.playback?.kind).toBe("PLAY");
  });

  it("a_message_that_fails_the_schema_changes_nothing", () => {
    const state = apply(initialRoomState, roomEvent({ type: "CLIENT_CHANGE", clients: [client("a")] }));
    const bad = [
      { type: "ERROR" },
      roomEvent({ type: "FUTURE_EVENT" }),
      roomEvent({ type: "CLIENT_CHANGE" }),
      null,
    ];
    for (const raw of bad) expect(applyServerMessage(state, raw)).toBe(state);
  });
});

describe("permissions", () => {
  it("can_mutate_admin_always_others_only_with_everyone", () => {
    const clients = [client("admin", true), client("guest")];
    const adminOnly: RoomState = { ...initialRoomState, clients, permissions: "ADMIN_ONLY" };
    const everyone: RoomState = { ...adminOnly, permissions: "EVERYONE" };
    expect(canMutate(adminOnly, "admin")).toBe(true);
    expect(canMutate(adminOnly, "guest")).toBe(false);
    expect(canMutate(adminOnly, "unknown")).toBe(false);
    expect(canMutate(everyone, "guest")).toBe(true);
    expect(isAdmin(adminOnly, "admin")).toBe(true);
    expect(isAdmin(everyone, "guest")).toBe(false);
  });
});
