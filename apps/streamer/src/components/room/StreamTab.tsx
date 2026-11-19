import { RefreshCw } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { toast } from "sonner";

import {
  type AppInfo,
  errorText,
  isIdle,
  isStreaming,
  listApps,
  saveSettings,
  startStream,
  stopStream,
  updateApps,
} from "@/api";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Switch } from "@/components/ui/switch";
import { useSessionStore } from "@/store/session";

const BROWSER_WARNING =
  "If beatsync is open in this browser, turn off 'Play live stream here' on that page, or the sound loops.";
const MIN_DELAY_MS = 250;
const MAX_DELAY_MS = 5000;

/** The stream controls of this PC: which apps to stream, the room delay, Start and Stop. */
export function StreamTab() {
  const settings = useSessionStore((s) => s.settings);
  const status = useSessionStore((s) => s.status);
  const setSettings = useSessionStore((s) => s.setSettings);

  const [apps, setApps] = useState<AppInfo[] | null>(null);
  const [appsError, setAppsError] = useState<string | null>(null);
  const [loadingApps, setLoadingApps] = useState(false);
  const [selected, setSelectedState] = useState<string[]>(settings.selectedApps);
  const selectedRef = useRef(selected);
  const [delay, setDelay] = useState(String(settings.delayMs));
  const [busy, setBusy] = useState(false);

  const idle = isIdle(status.phase);
  const idleRef = useRef(idle);
  idleRef.current = idle;

  const setSelected = useCallback((next: string[]) => {
    selectedRef.current = next;
    setSelectedState(next);
  }, []);

  /** Settings writes run one after another, in the order the user made them. */
  const queue = useRef<Promise<unknown>>(Promise.resolve());
  const enqueue = useCallback(<T,>(task: () => Promise<T>): Promise<T> => {
    const run = queue.current.then(task, task);
    queue.current = run.catch(() => undefined);
    return run;
  }, []);

  const refreshApps = useCallback(async () => {
    setLoadingApps(true);
    try {
      setApps(await listApps());
      setAppsError(null);
    } catch (e) {
      setApps(null);
      setAppsError(errorText(e));
    } finally {
      setLoadingApps(false);
    }
  }, []);

  useEffect(() => {
    void refreshApps();
  }, [refreshApps]);

  /** Saves the typed delay; true when the save worked or nothing changed. */
  const commitDelay = useCallback(
    () =>
      enqueue(async () => {
        const base = useSessionStore.getState().settings;
        const typed = Number.parseInt(delay, 10);
        const next = Number.isFinite(typed) ? Math.min(Math.max(typed, MIN_DELAY_MS), MAX_DELAY_MS) : base.delayMs;
        try {
          if (next !== base.delayMs) setSettings(await saveSettings({ ...base, delayMs: next }));
          setDelay(String(useSessionStore.getState().settings.delayMs));
          return true;
        } catch (e) {
          toast.error(errorText(e));
          return false;
        }
      }),
    [delay, enqueue, setSettings]
  );

  const toggle = (name: string, checked: boolean) => {
    const current = selectedRef.current;
    const next = checked ? [...current, name] : current.filter((n) => n !== name);
    setSelected(next);
    void enqueue(async () => {
      try {
        if (idleRef.current) {
          const base = useSessionStore.getState().settings;
          setSettings(await saveSettings({ ...base, selectedApps: next }));
        } else {
          setSettings(await updateApps(next));
        }
      } catch (e) {
        toast.error(errorText(e));
      }
    });
  };

  const start = async () => {
    setBusy(true);
    try {
      if (!(await commitDelay())) return;
      setSettings(await enqueue(() => startStream(selectedRef.current)));
    } catch (e) {
      toast.error(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  const stop = async () => {
    setBusy(true);
    try {
      await stopStream();
    } catch (e) {
      toast.error(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  // Selected apps that do not play now stay visible, so the user can unselect them.
  const absent = useMemo(() => {
    const listed = new Set((apps ?? []).map((app) => app.name));
    return selected.filter((name) => !listed.has(name));
  }, [apps, selected]);

  // Before the first sync the server offset can be unknown: a start would mute the apps
  // without a local copy.
  const canStart = idle && !busy && appsError === null && selected.length > 0 && status.syncedOnce;

  return (
    <ScrollArea className="h-full">
      <div className="space-y-4 p-3">
        <section className="space-y-2">
          <div className="flex items-center justify-between">
            <h2 className="text-sm font-medium text-neutral-300">Apps to stream</h2>
            <Button variant="ghost" size="sm" disabled={loadingApps} onClick={() => void refreshApps()}>
              <RefreshCw className={loadingApps ? "animate-spin" : undefined} />
              Refresh
            </Button>
          </div>
          {appsError !== null ? (
            <p className="text-sm text-red-400">{appsError}</p>
          ) : (
            <ul className="space-y-1">
              {apps === null && <li className="text-xs text-neutral-500">Looking for apps…</li>}
              {apps?.length === 0 && absent.length === 0 && (
                <li className="text-xs text-neutral-500">No app plays audio now.</li>
              )}
              {apps?.map((app) => {
                const checked = selected.includes(app.name);
                return (
                  <li key={app.name}>
                    <Label className="flex items-center justify-between gap-2 rounded-md px-1 py-1.5 text-sm text-neutral-200">
                      <span className="truncate">{app.name}</span>
                      <Switch checked={checked} onCheckedChange={(value) => toggle(app.name, value)} />
                    </Label>
                    {app.isBrowser && checked && <p className="px-1 text-xs text-amber-400">{BROWSER_WARNING}</p>}
                  </li>
                );
              })}
              {absent.map((name) => (
                <li key={name}>
                  <Label className="flex items-center justify-between gap-2 rounded-md px-1 py-1.5 text-sm text-neutral-200">
                    <span className="truncate">
                      {name} <span className="text-xs text-neutral-500">not playing</span>
                    </span>
                    <Switch checked onCheckedChange={() => toggle(name, false)} />
                  </Label>
                </li>
              ))}
            </ul>
          )}
        </section>

        <section className="space-y-1.5">
          <Label htmlFor="stream-delay" className="text-sm text-neutral-300">
            Room delay (ms)
          </Label>
          <Input
            id="stream-delay"
            type="number"
            min={MIN_DELAY_MS}
            max={MAX_DELAY_MS}
            step={50}
            value={delay}
            disabled={!idle}
            onChange={(e) => setDelay(e.target.value)}
            onBlur={() => void commitDelay()}
            onKeyDown={(e) => {
              if (e.key === "Enter") void commitDelay();
            }}
          />
        </section>

        {status.message !== null && <p className="text-sm text-red-400">{status.message}</p>}

        <section className="flex items-center gap-2">
          {idle ? (
            <Button disabled={!canStart} onClick={() => void start()}>
              Start stream
            </Button>
          ) : (
            <Button variant="destructive" disabled={busy} onClick={() => void stop()}>
              Stop stream
            </Button>
          )}
          {status.phase === "starting" && <span className="text-xs text-neutral-400">Starting…</span>}
          {isStreaming(status.phase) && <Badge className="bg-red-600 text-red-50">LIVE</Badge>}
          <span className="ml-auto text-xs text-neutral-400">Listeners: {status.listeners}</span>
        </section>
      </div>
    </ScrollArea>
  );
}
