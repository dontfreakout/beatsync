// Streamer clients (native live-stream source) never load queue tracks and have no admin UI. Loading coordination
// must not wait for them, and auto-promotion must never leave web users without an admin because of them.

import type { WSBroadcastType } from "@beatsync/shared";
import type { PlayActionType } from "@beatsync/shared/types/WSRequest";
import { afterEach, beforeEach, describe, expect, it, mock, vi } from "bun:test";
import { mockR2 } from "@/__tests__/mocks/r2";
import { createMockServer, createMockWs } from "@/__tests__/mocks/websocket";
import { RoomManager } from "@/managers/RoomManager";
import type { BunServer } from "@/utils/websocket";

let broadcastMessages: { server: BunServer; roomId: string; message: WSBroadcastType }[] = [];

mockR2();

void mock.module("@/utils/responses", () => ({
  sendBroadcast: mock(
    ({ server, roomId, message }: { server: BunServer; roomId: string; message: WSBroadcastType }) => {
      broadcastMessages.push({ server, roomId, message });
    }
  ),
  sendUnicast: mock(() => {
    /* noop */
  }),
  sendToClient: mock(() => {
    /* noop */
  }),
  corsHeaders: {},
  jsonResponse: mock(() => new Response()),
  errorResponse: mock(() => new Response()),
}));

const ROOM_ID = "test-room";
const AUDIO_URL = "https://example.com/song.mp3";

function join(room: RoomManager, clientId: string, kind: "web" | "streamer" = "web") {
  room.addClient(createMockWs({ clientId, kind }));
}

function isAdmin(room: RoomManager, clientId: string): boolean {
  return room.getClients().find((c) => c.clientId === clientId)?.isAdmin ?? false;
}

describe("Streamer clients", () => {
  let room: RoomManager;
  let server: BunServer;

  beforeEach(() => {
    vi.useFakeTimers();
    broadcastMessages = [];
    room = new RoomManager(ROOM_ID);
    room.addAudioSource({ url: AUDIO_URL });
    server = createMockServer();
  });

  afterEach(() => {
    room.clearAudioLoadingState();
    vi.useRealTimers();
  });

  it("PLAY does not wait for a streamer to load the track", () => {
    join(room, "A");
    join(room, "B");
    join(room, "S", "streamer");

    const play: PlayActionType = { type: "PLAY", audioSource: AUDIO_URL, trackTimeSeconds: 0 };
    room.initiateAudioSourceLoad(play, "A", server);
    room.processClientLoadedAudioSource("B", server);

    const scheduled = broadcastMessages.filter((m) => m.message.type === "SCHEDULED_ACTION");
    expect(scheduled).toHaveLength(1);
  });

  it("a streamer that joins first becomes admin", () => {
    join(room, "S", "streamer");
    expect(isAdmin(room, "S")).toBe(true);
  });

  it("a web client that joins a room where only the streamer is admin becomes admin", () => {
    join(room, "S", "streamer");
    join(room, "A");
    expect(isAdmin(room, "A")).toBe(true);
  });

  it("auto-promotion never picks a streamer, even with the newest lastSeenAt", () => {
    join(room, "A");
    join(room, "B");
    join(room, "S", "streamer");
    vi.advanceTimersByTime(1000); // fake clock: otherwise lastSeenAt ties and the pick is accidental
    room.markClientSeen("S");

    room.removeClient("A");

    expect(isAdmin(room, "B")).toBe(true);
    expect(isAdmin(room, "S")).toBe(false);
  });

  it("a room with only a streamer left promotes nobody", () => {
    join(room, "A");
    join(room, "S", "streamer");

    room.removeClient("A");

    expect(isAdmin(room, "S")).toBe(false);
  });
});
