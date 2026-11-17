import type { ExtractWSRequestFrom } from "@beatsync/shared";
import { encodeLiveFrame } from "@beatsync/shared";
import type { mock } from "bun:test";
import { beforeEach, describe, expect, it } from "bun:test";
import { createMockServer, createMockWs } from "@/__tests__/mocks/websocket";
import { globalManager } from "@/managers/GlobalManager";
import { handleMessage } from "@/routes/websocketHandlers";
import type { BunServer } from "@/utils/websocket";
import { relayLiveFrame } from "@/websocket/liveRelay";

const ROOM_ID = "relay-room";

const startMsg = {
  type: "START_LIVE_STREAM",
  codec: "opus",
  sampleRate: 48000,
  channels: 2,
  frameDurationMs: 20,
  delayMs: 1000,
  label: "x",
} as ExtractWSRequestFrom["START_LIVE_STREAM"];

const publishMock = (ws: unknown) => (ws as { publish: ReturnType<typeof mock> }).publish;

const frame = (streamId: number, sequence = 1) =>
  encodeLiveFrame({ streamId, sequence, captureServerTimeMs: 1000 }, new Uint8Array([1, 2, 3]));

describe("Live relay", () => {
  let server: BunServer;
  let sourceWs: ReturnType<typeof createMockWs>;
  let otherWs: ReturnType<typeof createMockWs>;
  const room = () => globalManager.getRoom(ROOM_ID)!;

  beforeEach(() => {
    for (const id of globalManager.getRoomIds()) globalManager.deleteRoom(id);
    server = createMockServer();
    const r = globalManager.getOrCreateRoom(ROOM_ID);
    sourceWs = createMockWs({ clientId: "source", roomId: ROOM_ID });
    otherWs = createMockWs({ clientId: "other", roomId: ROOM_ID });
    r.addClient(sourceWs);
    r.addClient(otherWs);
  });

  const start = () => room().startLiveStream({ sourceClientId: "source", request: startMsg, server });

  it("forwards a frame from the source with the current streamId via ws.publish", () => {
    const stream = start();
    const data = frame(stream.streamId);
    (server.publish as ReturnType<typeof mock>).mockClear();
    expect(relayLiveFrame(sourceWs, data)).toBe(true);
    expect(publishMock(sourceWs)).toHaveBeenCalledTimes(1);
    expect(publishMock(sourceWs)).toHaveBeenCalledWith(ROOM_ID, data);
    expect(publishMock(server)).not.toHaveBeenCalled();
  });

  it("drops frames from a client that is not the source", () => {
    const stream = start();
    expect(relayLiveFrame(otherWs, frame(stream.streamId))).toBe(false);
    expect(publishMock(otherWs)).not.toHaveBeenCalled();
  });

  it("drops frames with a stale streamId", () => {
    const first = start();
    room().startLiveStream({ sourceClientId: "source", request: startMsg, server });
    expect(relayLiveFrame(sourceWs, frame(first.streamId))).toBe(false);
    expect(publishMock(sourceWs)).not.toHaveBeenCalled();
  });

  it("drops frames when no stream is live", () => {
    expect(relayLiveFrame(sourceWs, frame(1))).toBe(false);
    expect(publishMock(sourceWs)).not.toHaveBeenCalled();
  });

  it("drops short or foreign-kind frames", () => {
    const stream = start();
    expect(relayLiveFrame(sourceWs, new Uint8Array(5))).toBe(false);
    const foreign = frame(stream.streamId);
    foreign[0] = 2;
    expect(relayLiveFrame(sourceWs, foreign)).toBe(false);
    expect(publishMock(sourceWs)).not.toHaveBeenCalled();
  });

  it("a binary message marks the sender as seen", async () => {
    const stream = start();
    const client = room().getClient("source")!;
    client.lastSeenAt = 0;
    await handleMessage(sourceWs, Buffer.from(frame(stream.streamId)), server);
    expect(room().getClient("source")!.lastSeenAt).toBeGreaterThan(0);
    const errors = (sourceWs.send as ReturnType<typeof mock>).mock.calls.filter((c) => String(c[0]).includes("ERROR"));
    expect(errors).toHaveLength(0);
    expect(publishMock(sourceWs)).toHaveBeenCalledTimes(1);
  });
});
