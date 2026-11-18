import { describe, expect, test } from "bun:test";

import { isRoomCode, randomRoomCode } from "./room";

describe("room codes", () => {
  test("random codes stay in 100000..999999", () => {
    expect(randomRoomCode(() => 0)).toBe("100000");
    expect(randomRoomCode(() => 0.9999999)).toBe("999999");
  });
  test("accepts exactly six digits", () => {
    expect(isRoomCode("123456")).toBe(true);
    for (const bad of ["12345", "1234567", "12a456", " 123456"]) expect(isRoomCode(bad)).toBe(false);
  });
});
