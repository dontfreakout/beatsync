const ROOM_CODE = /^\d{6}$/;

export function isRoomCode(s: string): boolean {
  return ROOM_CODE.test(s);
}

/** A random 6-digit room code, as the web client's "Create new room". */
export function randomRoomCode(rand: () => number = Math.random): string {
  return Math.floor(100000 + rand() * 900000).toString();
}
