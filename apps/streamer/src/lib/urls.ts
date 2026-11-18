/**
 * The web client's address: a set `webUrl` wins; else it derives from the server's WebSocket
 * address (`ws` -> `http`, `wss` -> `https`, no `/ws` path). No query, no trailing slash.
 * Null if unparsable or not `http`/`https` (e.g. "beatsync.gg" without a scheme).
 */
export function effectiveWebUrl(serverUrl: string, webUrl: string): string | null {
  const set = webUrl.trim();
  let url: URL;
  try {
    url = new URL(set || serverUrl.trim());
  } catch {
    return null;
  }
  if (!set) {
    if (url.protocol === "ws:") url.protocol = "http:";
    else if (url.protocol === "wss:") url.protocol = "https:";
    url.pathname = url.pathname.replace(/\/ws\/?$/, "");
  }
  if (url.protocol !== "http:" && url.protocol !== "https:") return null;
  url.search = "";
  url.hash = "";
  return url.toString().replace(/\/+$/, "");
}

export function roomLink(webBase: string, roomId: string): string {
  return `${webBase}/room/${roomId}`;
}

/** The host (with port) of `url`, for display; `url` itself when it does not parse. */
export function hostOf(url: string): string {
  try {
    return new URL(url).host;
  } catch {
    return url;
  }
}
