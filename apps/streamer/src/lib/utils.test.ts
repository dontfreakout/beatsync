import { describe, expect, it } from "bun:test";
import { R2_AUDIO_FILE_NAME_DELIMITER } from "@beatsync/shared";

import { extractFileNameFromUrl } from "./utils";

describe("extractFileNameFromUrl", () => {
  it("returns_the_original_name_before_the_r2_delimiter", () => {
    const url = `https://cdn.example.com/room-123456/My%20Song${R2_AUDIO_FILE_NAME_DELIMITER}1700000000.mp3`;
    expect(extractFileNameFromUrl(url)).toBe("My Song");
  });

  it("trims_the_extension_of_a_plain_url", () => {
    expect(extractFileNameFromUrl("https://example.com/audio/default%20track.mp3")).toBe("default track");
  });
});
