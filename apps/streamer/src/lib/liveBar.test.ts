import { describe, expect, test } from "bun:test";

import { liveBarMode } from "./liveBar";

const stream = (sourceClientId: string) => ({ sourceClientId }) as never;

describe("liveBarMode", () => {
  test("this PC streams: own badge with Stop", () => {
    expect(liveBarMode(true, stream("me"), "me")).toBe("own-with-stop");
    expect(liveBarMode(true, null, "me")).toBe("own-with-stop");
  });

  test("the server names this PC but the app is idle or starting: own badge, no Stop", () => {
    expect(liveBarMode(false, stream("me"), "me")).toBe("own");
  });

  test("another client streams", () => {
    expect(liveBarMode(false, stream("other"), "me")).toBe("other");
    expect(liveBarMode(true, stream("other"), "me")).toBe("own-with-stop");
  });

  test("no stream", () => {
    expect(liveBarMode(false, null, "me")).toBe("none");
  });
});
