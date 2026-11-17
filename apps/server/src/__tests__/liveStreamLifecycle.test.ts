import type { ExtractWSRequestFrom, WSBroadcastType, WSUnicastType } from "@beatsync/shared";
import { epochNow } from "@beatsync/shared";
import { beforeEach, describe, expect, it, mock } from "bun:test";
import { createMockServer, createMockWs } from "@/__tests__/mocks/websocket";
import { globalManager } from "@/managers/GlobalManager";
import { handleClose, handleOpen } from "@/routes/websocketHandlers";
import type { BunServer } from "@/utils/websocket";
import { handlePlay } from "@/websocket/handlers/play";
import { handleStartLiveStream } from "@/websocket/handlers/startLiveStream";
import { handleStopLiveStream } from "@/websocket/handlers/stopLiveStream";
import { handleUpdateLiveStream } from "@/websocket/handlers/updateLiveStream";

let broadcasts: WSBroadcastType[] = [];
let toClient: { clientId: string; message: WSBroadcastType }[] = [];
let unicasts: { clientId: string; message: WSUnicastType }[] = [];

void mock.module("@/utils/responses", () => ({
  sendBroadcast: mock(({ message }: { message: WSBroadcastType }) => {
    broadcasts.push(message);
  }),
  sendToClient: mock(({ ws, message }: { ws: ReturnType<typeof createMockWs>; message: WSBroadcastType }) => {
    toClient.push({ clientId: ws.data.clientId, message });
  }),
  sendUnicast: mock(({ ws, message }: { ws: ReturnType<typeof createMockWs>; message: WSUnicastType }) => {
    unicasts.push({ clientId: ws.data.clientId, message });
  }),
  corsHeaders: {},
  jsonResponse: mock(() => new Response()),
  errorResponse: mock(() => new Response()),
}));

const lastOf = <T>(items: T[]): T | undefined => items[items.length - 1];

const ROOM_ID = "live-room";

const startMsg = (overrides: Partial<ExtractWSRequestFrom["START_LIVE_STREAM"]> = {}) =>
  ({
    type: "START_LIVE_STREAM",
    codec: "opus",
    sampleRate: 48000,
    channels: 2,
    frameDurationMs: 20,
    delayMs: 1000,
    label: "Spotify",
    ...overrides,
  }) as ExtractWSRequestFrom["START_LIVE_STREAM"];

const liveStateBroadcasts = () =>
  broadcasts.filter(
    (m): m is Extract<WSBroadcastType, { type: "ROOM_EVENT" }> =>
      m.type === "ROOM_EVENT" && m.event.type === "LIVE_STREAM_STATE"
  );

describe("Live stream lifecycle", () => {
  let server: BunServer;
  let adminWs: ReturnType<typeof createMockWs>;
  let otherWs: ReturnType<typeof createMockWs>;

  const room = () => globalManager.getRoom(ROOM_ID)!;

  beforeEach(() => {
    broadcasts = [];
    toClient = [];
    unicasts = [];
    for (const id of globalManager.getRoomIds()) globalManager.deleteRoom(id);
    server = createMockServer();
    const r = globalManager.getOrCreateRoom(ROOM_ID);
    adminWs = createMockWs({ clientId: "admin", roomId: ROOM_ID });
    otherWs = createMockWs({ clientId: "other", roomId: ROOM_ID });
    r.addClient(adminWs); // first client is admin
    r.addClient(otherWs);
  });

  it("a late close of an old socket does not end the stream of the reconnected source", async () => {
    await handleStartLiveStream({ ws: adminWs, message: startMsg(), server });
    const ws2 = createMockWs({ clientId: "admin", roomId: ROOM_ID });
    room().addClient(ws2);
    handleClose(adminWs, server);
    expect(room().getLiveStream()).not.toBeNull();
    expect(
      room()
        .getClients()
        .some((c) => c.clientId === "admin")
    ).toBe(true);
    handleClose(ws2, server);
    const last = lastOf(liveStateBroadcasts())!;
    if (last.event.type !== "LIVE_STREAM_STATE") throw new Error("unreachable");
    expect(last.event.stream).toBeNull();
  });

  it("closing the current socket still removes the client", () => {
    handleClose(otherWs, server);
    expect(
      room()
        .getClients()
        .some((c) => c.clientId === "other")
    ).toBe(false);
  });

  it("rejects START from a client without playback permission", async () => {
    await handleStartLiveStream({ ws: otherWs, message: startMsg(), server });
    expect(lastOf(unicasts)?.message).toEqual({ type: "LIVE_STREAM_REJECTED", reason: "NOT_ALLOWED" });
    expect(room().getLiveStream()).toBeNull();
  });

  it("rejects START while another client streams", async () => {
    room().setPlaybackControls("EVERYONE");
    await handleStartLiveStream({ ws: adminWs, message: startMsg(), server });
    await handleStartLiveStream({ ws: otherWs, message: startMsg(), server });
    expect(lastOf(unicasts)).toEqual({
      clientId: "other",
      message: { type: "LIVE_STREAM_REJECTED", reason: "ALREADY_ACTIVE" },
    });
    expect(room().getLiveStream()?.sourceClientId).toBe("admin");
  });

  it("a repeated START from the source replaces its stream with a new streamId", async () => {
    await handleStartLiveStream({ ws: adminWs, message: startMsg(), server });
    const first = room().getLiveStream()!.streamId;
    await handleStartLiveStream({ ws: adminWs, message: startMsg({ label: "Again" }), server });
    const second = room().getLiveStream()!;
    expect(second.streamId).toBeGreaterThan(first);
    expect(second.label).toBe("Again");
    expect(liveStateBroadcasts()).toHaveLength(2);
  });

  it("a START after the source rejoins gets a new streamId", async () => {
    await handleStartLiveStream({ ws: adminWs, message: startMsg(), server });
    const a = room().getLiveStream()!.streamId;
    room().removeClient("admin");
    room().addClient(adminWs);
    await handleStartLiveStream({ ws: adminWs, message: startMsg(), server });
    const b = room().getLiveStream()!.streamId;
    expect(b).not.toBe(a);
  });

  it("START pauses a playing queue at the current track position", async () => {
    const url = "https://example.com/a.mp3";
    room().addAudioSource({ url });
    const t0 = epochNow() - 5000;
    room().updatePlaybackSchedulePlay({ type: "PLAY", audioSource: url, trackTimeSeconds: 10 }, t0);
    await handleStartLiveStream({ ws: adminWs, message: startMsg(), server });
    const scheduled = broadcasts.find((m) => m.type === "SCHEDULED_ACTION");
    if (scheduled?.type !== "SCHEDULED_ACTION" || scheduled.scheduledAction.type !== "PAUSE") {
      throw new Error("Expected a PAUSE scheduled action");
    }
    expect(scheduled.scheduledAction.audioSource).toBe(url);
    expect(scheduled.scheduledAction.trackTimeSeconds).toBeCloseTo(10 + (scheduled.serverTimeToExecute - t0) / 1000, 3);
    expect(room().getPlaybackState().type).toBe("paused");
  });

  it("PLAY is refused while a stream is live", async () => {
    const url = "https://example.com/a.mp3";
    room().addAudioSource({ url });
    await handleStartLiveStream({ ws: adminWs, message: startMsg(), server });
    broadcasts = [];
    await handlePlay({ ws: adminWs, message: { type: "PLAY", audioSource: url, trackTimeSeconds: 0 }, server });
    expect(broadcasts.some((m) => m.type === "ROOM_EVENT" && m.event.type === "LOAD_AUDIO_SOURCE")).toBe(false);
  });

  it("removing the source client ends the stream and broadcasts stream: null", async () => {
    await handleStartLiveStream({ ws: adminWs, message: startMsg(), server });
    room().removeClient("admin");
    const last = lastOf(liveStateBroadcasts())!;
    if (last.event.type !== "LIVE_STREAM_STATE") throw new Error("unreachable");
    expect(last.event.stream).toBeNull();
    expect(room().getLiveStream()).toBeNull();
  });

  it("STOP from a non-source client with permission sets stoppedByClientId", async () => {
    room().setPlaybackControls("EVERYONE");
    await handleStartLiveStream({ ws: adminWs, message: startMsg(), server });
    await handleStopLiveStream({ ws: otherWs, message: { type: "STOP_LIVE_STREAM" }, server });
    const last = lastOf(liveStateBroadcasts())!;
    if (last.event.type !== "LIVE_STREAM_STATE") throw new Error("unreachable");
    expect(last.event.stream).toBeNull();
    expect(last.event.stoppedByClientId).toBe("other");
    expect(room().getLiveStream()).toBeNull();
  });

  it("STOP from the source omits stoppedByClientId", async () => {
    await handleStartLiveStream({ ws: adminWs, message: startMsg(), server });
    await handleStopLiveStream({ ws: adminWs, message: { type: "STOP_LIVE_STREAM" }, server });
    const last = lastOf(liveStateBroadcasts())!;
    if (last.event.type !== "LIVE_STREAM_STATE") throw new Error("unreachable");
    expect(last.event.stoppedByClientId).toBeUndefined();
  });

  it("STOP from a client without permission is ignored", async () => {
    await handleStartLiveStream({ ws: adminWs, message: startMsg(), server });
    await handleStopLiveStream({ ws: otherWs, message: { type: "STOP_LIVE_STREAM" }, server });
    expect(room().getLiveStream()).not.toBeNull();
  });

  it("UPDATE from the source changes the label; from a non-source it is ignored", async () => {
    room().setPlaybackControls("EVERYONE");
    await handleStartLiveStream({ ws: adminWs, message: startMsg(), server });
    await handleUpdateLiveStream({ ws: otherWs, message: { type: "UPDATE_LIVE_STREAM", label: "Hacked" }, server });
    expect(room().getLiveStream()?.label).toBe("Spotify");
    expect(liveStateBroadcasts()).toHaveLength(1);
    await handleUpdateLiveStream({ ws: adminWs, message: { type: "UPDATE_LIVE_STREAM", label: "New" }, server });
    expect(room().getLiveStream()?.label).toBe("New");
    expect(liveStateBroadcasts()).toHaveLength(2);
  });

  it("a client that joins during a stream receives LIVE_STREAM_STATE", async () => {
    await handleStartLiveStream({ ws: adminWs, message: startMsg(), server });
    const id = room().getLiveStream()!.streamId;
    const ws2 = createMockWs({ clientId: "late", roomId: ROOM_ID });
    handleOpen(ws2, server);
    const msg = toClient.find(
      (m) => m.clientId === "late" && m.message.type === "ROOM_EVENT" && m.message.event.type === "LIVE_STREAM_STATE"
    )?.message;
    if (msg?.type !== "ROOM_EVENT" || msg.event.type !== "LIVE_STREAM_STATE") throw new Error("missing state");
    expect(msg.event.stream?.streamId).toBe(id);
  });

  it("a client that joins with no active stream receives LIVE_STREAM_STATE with stream: null", () => {
    const ws2 = createMockWs({ clientId: "late", roomId: ROOM_ID });
    handleOpen(ws2, server);
    const msg = toClient.find(
      (m) => m.clientId === "late" && m.message.type === "ROOM_EVENT" && m.message.event.type === "LIVE_STREAM_STATE"
    )?.message;
    if (msg?.type !== "ROOM_EVENT" || msg.event.type !== "LIVE_STREAM_STATE") throw new Error("missing state");
    expect(msg.event.stream).toBeNull();
  });
});
