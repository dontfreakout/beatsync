/** The audio extensions the native file dialog offers and a dropped file may have. */
export const AUDIO_EXTENSIONS = ["mp3", "wav", "m4a", "aac", "ogg", "webm", "flac"];

const baseName = (path: string) => path.split(/[\\/]/).pop() ?? path;

/** True when the path ends in one of `AUDIO_EXTENSIONS` (case-insensitive). */
export function isAudioPath(path: string): boolean {
  const name = baseName(path);
  const dot = name.lastIndexOf(".");
  return dot > 0 && AUDIO_EXTENSIONS.includes(name.slice(dot + 1).toLowerCase());
}

/** True when a drag position in physical pixels is inside a rect in CSS pixels. */
export function isInsideRect(
  position: { x: number; y: number },
  pixelRatio: number,
  rect: { left: number; top: number; right: number; bottom: number },
): boolean {
  const x = position.x / pixelRatio;
  const y = position.y / pixelRatio;
  return x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom;
}

/** The file name of a path without its extension, as the web shows an uploaded file. */
export const uploadLabel = (path: string): string => baseName(path).replace(/\.[^/.]+$/, "");
