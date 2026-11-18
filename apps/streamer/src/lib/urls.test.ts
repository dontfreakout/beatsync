import { describe, expect, test } from "bun:test";

import { effectiveWebUrl, hostOf, roomLink } from "./urls";

describe("urls", () => {
  test("derives the web address from the server address", () => {
    expect(effectiveWebUrl("ws://10.0.0.5:8091/ws", "")).toBe("http://10.0.0.5:8091");
    expect(effectiveWebUrl("wss://beatsync.example/ws?x=1", "")).toBe("https://beatsync.example");
  });
  test("a set web address wins and loses its trailing slash", () => {
    expect(effectiveWebUrl("ws://h/ws", "http://h:3000/")).toBe("http://h:3000");
  });
  test("a set web address without an http or https scheme gives null", () => {
    // The QR dialog parses the address; "beatsync.gg" blanked the window (no error boundary).
    expect(effectiveWebUrl("ws://h/ws", "beatsync.gg")).toBeNull();
    expect(effectiveWebUrl("ws://h/ws", "192.168.1.5:3000")).toBeNull();
    expect(effectiveWebUrl("ws://h/ws", "ftp://x")).toBeNull();
  });
  test("an unparsable server address gives null", () => {
    expect(effectiveWebUrl("not a url", "")).toBeNull();
  });
  test("builds the room link", () => {
    expect(roomLink("http://h:3000", "123456")).toBe("http://h:3000/room/123456");
  });
  test("the host of an address never throws", () => {
    expect(hostOf("http://h:3000/room/123456")).toBe("h:3000");
    expect(hostOf("beatsync.gg")).toBe("beatsync.gg");
  });
});
