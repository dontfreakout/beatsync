import { describe, expect, it } from "bun:test";
import fixtures from "../fixtures/protocol-messages.json";
import { WSRequestSchema } from "../types/WSRequest";
import { WSResponseSchema } from "../types/WSResponse";

describe("protocol message fixtures", () => {
  it("every clientToServer example is a valid WSRequest", () => {
    for (const { name, message } of fixtures.clientToServer) {
      const parsed = WSRequestSchema.parse(message);
      // Deep equality: no fixture field is stripped as unknown or filled in by a default.
      expect({ name, parsed }).toEqual({ name, parsed: message });
    }
  });

  it("every serverToClient example is a valid WSResponse", () => {
    for (const { name, message } of fixtures.serverToClient) {
      const parsed = WSResponseSchema.parse(message);
      expect({ name, parsed }).toEqual({ name, parsed: message });
    }
  });

  it("names are unique", () => {
    for (const list of [fixtures.clientToServer, fixtures.serverToClient]) {
      const names = list.map((e) => e.name);
      expect(new Set(names).size).toBe(names.length);
    }
  });
});
