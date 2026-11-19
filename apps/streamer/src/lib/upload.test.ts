import { describe, expect, it } from "bun:test";

import { AUDIO_EXTENSIONS, isAudioPath, isInsideRect, uploadLabel } from "./upload";

describe("isAudioPath", () => {
  it("accepts_every_dialog_extension_in_any_case", () => {
    for (const ext of AUDIO_EXTENSIONS) {
      expect(isAudioPath(`/music/a.${ext}`)).toBe(true);
      expect(isAudioPath(`C:\\music\\a.${ext.toUpperCase()}`)).toBe(true);
    }
  });

  it("refuses_other_or_missing_extensions", () => {
    expect(isAudioPath("/music/a.txt")).toBe(false);
    expect(isAudioPath("/music/mp3")).toBe(false);
    expect(isAudioPath("/music.mp3/readme")).toBe(false);
  });
});

describe("isInsideRect", () => {
  const rect = { left: 10, top: 20, right: 110, bottom: 70 };

  it("divides_physical_pixels_by_the_pixel_ratio", () => {
    expect(isInsideRect({ x: 40, y: 80 }, 2, rect)).toBe(true);
    expect(isInsideRect({ x: 40, y: 80 }, 1, rect)).toBe(false);
  });

  it("is_false_outside_the_rect", () => {
    expect(isInsideRect({ x: 5, y: 30 }, 1, rect)).toBe(false);
    expect(isInsideRect({ x: 50, y: 71 }, 1, rect)).toBe(false);
  });
});

describe("uploadLabel", () => {
  it("shows_the_trimmed_base_name_of_a_path", () => {
    expect(uploadLabel("/music/My Song.mp3")).toBe("My Song");
    expect(uploadLabel("C:\\music\\other.v2.FLAC")).toBe("other.v2");
  });
});
