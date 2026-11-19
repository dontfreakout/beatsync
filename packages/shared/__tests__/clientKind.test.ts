import { describe, expect, it } from "bun:test";
import { ClientDataSchema } from "../types/WSBroadcast";

const base = { username: "a", clientId: "1", position: { x: 50, y: 50 }, joinedAt: 0 };

describe("client kind", () => {
  it("parses app", () => expect(ClientDataSchema.parse({ ...base, kind: "app" }).kind).toBe("app"));
  it("falls back to web for an unknown kind", () =>
    expect(ClientDataSchema.parse({ ...base, kind: "tv" }).kind).toBe("web"));
  it("defaults to web", () => expect(ClientDataSchema.parse(base).kind).toBe("web"));
});
