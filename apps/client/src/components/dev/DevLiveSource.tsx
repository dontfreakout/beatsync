"use client";
import { DevLiveSource as Source } from "@/lib/liveStream/devSource";
import { LIVE_STREAM_CONSTANTS } from "@beatsync/shared";
import { useRef, useState } from "react";

export function DevLiveSource() {
  const [roomId, setRoomId] = useState("123456");
  const [username, setUsername] = useState("dev-source");
  const [delayMs, setDelayMs] = useState<number>(LIVE_STREAM_CONSTANTS.DEFAULT_DELAY_MS);
  const [kind, setKind] = useState<"click" | "file">("click");
  const [file, setFile] = useState<File | null>(null);
  const [status, setStatus] = useState("idle");
  const [running, setRunning] = useState(false);
  const sourceRef = useRef<Source | null>(null);

  const start = async () => {
    sourceRef.current?.stop();
    const source = new Source({ roomId, username, delayMs, onStatus: setStatus });
    sourceRef.current = source;
    setRunning(true);
    try {
      if (kind === "file") {
        if (!file) throw new Error("Choose an audio file");
        await source.start({ kind: "file", file });
      } else {
        await source.start({ kind: "click" });
      }
    } catch (e) {
      setStatus(`error: ${e instanceof Error ? e.message : String(e)}`);
      source.stop();
      setRunning(false);
    }
  };

  const stop = () => {
    sourceRef.current?.stop();
    sourceRef.current = null;
    setRunning(false);
    setStatus("stopped");
  };

  return (
    <div className="mx-auto max-w-md space-y-3 p-6 text-sm">
      <h1 className="text-lg font-semibold">Dev live stream source</h1>
      <label className="block">
        Room code
        <input
          className="mt-1 w-full rounded border bg-transparent p-1"
          value={roomId}
          onChange={(e) => setRoomId(e.target.value)}
        />
      </label>
      <label className="block">
        Name
        <input
          className="mt-1 w-full rounded border bg-transparent p-1"
          value={username}
          onChange={(e) => setUsername(e.target.value)}
        />
      </label>
      <label className="block">
        Delay (ms)
        <input
          type="number"
          className="mt-1 w-full rounded border bg-transparent p-1"
          value={delayMs}
          min={LIVE_STREAM_CONSTANTS.MIN_DELAY_MS}
          max={LIVE_STREAM_CONSTANTS.MAX_DELAY_MS}
          onChange={(e) => setDelayMs(Number(e.target.value))}
        />
      </label>
      <label className="block">
        Source
        <select
          className="mt-1 w-full rounded border bg-transparent p-1"
          value={kind}
          onChange={(e) => setKind(e.target.value as "click" | "file")}
        >
          <option value="click">Click track</option>
          <option value="file">Audio file (looped)</option>
        </select>
      </label>
      {kind === "file" && <input type="file" accept="audio/*" onChange={(e) => setFile(e.target.files?.[0] ?? null)} />}
      <div className="flex gap-2">
        <button className="rounded border px-3 py-1" disabled={running} onClick={start}>
          Start
        </button>
        <button className="rounded border px-3 py-1" disabled={!running} onClick={stop}>
          Stop
        </button>
      </div>
      <p data-testid="status">Status: {status}</p>
      <p className="text-neutral-500">Keep this tab in the foreground.</p>
    </div>
  );
}
