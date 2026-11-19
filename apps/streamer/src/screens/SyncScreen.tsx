import { useState } from "react";

import { MAX_SYNC_MEASUREMENTS, errorText } from "@/api";
import { Button } from "@/components/ui/button";
import { leaveAndReset } from "@/lib/leave";
import { useSessionStore } from "@/store/session";

const PILL_COUNT = 8;
const MEASUREMENTS_PER_PILL = MAX_SYNC_MEASUREMENTS / PILL_COUNT;

export function SyncScreen() {
  const status = useSessionStore((s) => s.status);
  const [error, setError] = useState<string | null>(null);
  const [leaving, setLeaving] = useState(false);

  const litPills = Math.min(PILL_COUNT, Math.floor(status.syncMeasurements / MEASUREMENTS_PER_PILL));
  const dot = status.connected ? "34,197,94" : "234,179,8";

  return (
    <div className="flex h-screen flex-col items-center justify-center bg-neutral-950">
      <div className="w-full max-w-md px-1">
        <div className="flex flex-col items-center justify-center rounded-md border border-neutral-800 bg-neutral-900 p-6 shadow-lg">
          <h2 className="relative mb-1 text-base font-medium tracking-tight text-white">
            <span className="absolute -left-5 top-1/2 flex size-2 -translate-y-1/2">
              <span
                className="relative inline-flex size-2 rounded-full"
                style={{ backgroundColor: `rgb(${dot})`, boxShadow: `0 0 6px 1px rgba(${dot},0.5)` }}
              />
            </span>
            Beatsync calibrating
          </h2>
          <p className="mb-5 text-center text-sm text-neutral-400">
            {status.connected ? "Synchronizing time..." : "Connecting..."}
          </p>

          <div className="mb-3 mt-4 flex w-full gap-2">
            {Array.from({ length: PILL_COUNT }, (_, i) => {
              const lit = litPills > i;
              return (
                <div
                  key={i}
                  className="h-[3px] flex-1 rounded-full"
                  style={{
                    backgroundColor: lit ? "#ffffff" : "rgba(255, 255, 255, 0.08)",
                    boxShadow: lit
                      ? "0 0 8px 2px rgba(255, 255, 255, 0.7), 0 0 20px 4px rgba(255, 255, 255, 0.25)"
                      : "none",
                    transition: "box-shadow 0.15s ease-out",
                  }}
                />
              );
            })}
          </div>

          <div className="mt-3 w-full font-mono text-[10px] leading-relaxed text-neutral-500">
            <div className="flex justify-between">
              <span>measurements</span>
              <span className="text-neutral-400">
                {status.syncMeasurements} / {MAX_SYNC_MEASUREMENTS}
              </span>
            </div>
          </div>

          {status.message && <p className="mt-3 text-center text-xs text-neutral-400">{status.message}</p>}
          {error && <p className="mt-3 text-center text-xs text-red-500">{error}</p>}

          <Button
            variant="ghost"
            className="mt-4 w-full rounded-full text-xs text-neutral-400"
            disabled={leaving}
            onClick={() => {
              setError(null);
              setLeaving(true);
              leaveAndReset()
                .catch((e) => setError(errorText(e)))
                .finally(() => setLeaving(false));
            }}
          >
            {leaving ? "Leaving…" : "Leave"}
          </Button>
        </div>
      </div>
    </div>
  );
}
