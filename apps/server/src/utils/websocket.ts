import type { ClientKind } from "@beatsync/shared";
import type { Server } from "bun";

export interface WSData {
  roomId: string;
  clientId: string;
  username: string;
  isAdmin: boolean;
  isCreator: boolean;
  kind: ClientKind;
}

export function parseClientKind(value: string | null): ClientKind {
  return value === "streamer" || value === "app" ? value : "web";
}

export type BunServer = Server<WSData>;
