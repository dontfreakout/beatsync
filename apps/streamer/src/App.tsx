import { type KeyboardEvent, useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  type AppInfo,
  type Settings,
  type Status,
  errorText,
  getSettings,
  isIdle,
  isStreaming,
  listApps,
  saveSettings,
  startStream,
  stopStream,
  subscribeStatus,
  updateApps,
} from "./api";

const BROWSER_WARNING =
  "If beatsync is open in this browser, turn off 'Play live stream here' on that page, or the sound loops.";
const MIN_DELAY_MS = 250;
const MAX_DELAY_MS = 5000;

const INITIAL_STATUS: Status = {
  connected: false,
  synced: false,
  syncedOnce: false,
  rttMs: null,
  listeners: 0,
  phase: "idle",
  message: null,
};

/** The settings fields as typed. */
interface Draft {
  serverUrl: string;
  roomId: string;
  username: string;
  delayMs: string;
  localOffsetMs: string;
}

const DRAFT_KEYS = ["serverUrl", "roomId", "username", "delayMs", "localOffsetMs"] as const;

function toDraft(s: Settings): Draft {
  return {
    serverUrl: s.serverUrl,
    roomId: s.roomId,
    username: s.username,
    delayMs: String(s.delayMs),
    localOffsetMs: String(s.localOffsetMs),
  };
}

/** Applies the typed fields to `base`; a number field that does not parse keeps its value. */
function fromDraft(base: Settings, draft: Draft): Settings {
  const delay = Number.parseInt(draft.delayMs, 10);
  const offset = Number.parseFloat(draft.localOffsetMs);
  return {
    ...base,
    serverUrl: draft.serverUrl.trim(),
    roomId: draft.roomId.trim(),
    username: draft.username.trim(),
    delayMs: Number.isFinite(delay) ? Math.min(Math.max(delay, MIN_DELAY_MS), MAX_DELAY_MS) : base.delayMs,
    localOffsetMs: Number.isFinite(offset) ? offset : base.localOffsetMs,
  };
}

function sameSettings(a: Settings, b: Settings): boolean {
  return (
    DRAFT_KEYS.every((key) => a[key] === b[key]) &&
    a.selectedApps.length === b.selectedApps.length &&
    a.selectedApps.every((name, i) => name === b.selectedApps[i])
  );
}

/** Shows the saved value in each field that the user has not edited since `sent`. */
function settle(current: Draft, sent: Draft, saved: Draft): Draft {
  const next = { ...current };
  for (const key of DRAFT_KEYS) {
    if (current[key] === sent[key]) next[key] = saved[key];
  }
  return next;
}

export function App() {
  const [settings, setSettingsState] = useState<Settings | null>(null);
  const settingsRef = useRef<Settings | null>(null);
  const [draft, setDraftState] = useState<Draft | null>(null);
  const draftRef = useRef<Draft | null>(null);
  const [selected, setSelectedState] = useState<string[]>([]);
  const selectedRef = useRef<string[]>([]);
  const [status, setStatusState] = useState<Status>(INITIAL_STATUS);
  const statusRef = useRef<Status>(INITIAL_STATUS);

  const [apps, setApps] = useState<AppInfo[] | null>(null);
  const [appsError, setAppsError] = useState<string | null>(null);
  const [loadingApps, setLoadingApps] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // Refs hold the latest values for the async handlers below.
  const setSettings = useCallback((next: Settings) => {
    settingsRef.current = next;
    setSettingsState(next);
  }, []);
  const setDraft = useCallback((next: Draft) => {
    draftRef.current = next;
    setDraftState(next);
  }, []);
  const setSelected = useCallback((next: string[]) => {
    selectedRef.current = next;
    setSelectedState(next);
  }, []);
  const setStatus = useCallback((next: Status) => {
    statusRef.current = next;
    setStatusState(next);
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

  // Once, also under StrictMode: a second status subscription would replace the first.
  const started = useRef(false);
  useEffect(() => {
    if (started.current) return;
    started.current = true;
    getSettings()
      .then((saved) => {
        setSettings(saved);
        setDraft(toDraft(saved));
        setSelected(saved.selectedApps);
      })
      .catch((e) => setError(errorText(e)));
    subscribeStatus(setStatus).catch((e) => setError(errorText(e)));
    void refreshApps();
  }, [refreshApps, setDraft, setSelected, setSettings, setStatus]);

  /** Saves the typed fields with the current selection; false when the save failed. */
  const commit = useCallback(
    () =>
      enqueue(async () => {
        const base = settingsRef.current;
        const sent = draftRef.current;
        if (!base || !sent) return true;
        const next = { ...fromDraft(base, sent), selectedApps: selectedRef.current };
        try {
          let saved = base;
          if (!sameSettings(next, base)) {
            saved = await saveSettings(next);
            setSettings(saved);
            setError(null);
          }
          if (draftRef.current) setDraft(settle(draftRef.current, sent, toDraft(saved)));
          return true;
        } catch (e) {
          setError(errorText(e));
          return false;
        }
      }),
    [enqueue, setDraft, setSettings]
  );

  const edit = (key: (typeof DRAFT_KEYS)[number], value: string) => {
    if (draftRef.current) setDraft({ ...draftRef.current, [key]: value });
  };
  const commitOnEnter = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "Enter") void commit();
  };

  const toggle = (name: string, checked: boolean) => {
    const current = selectedRef.current;
    const next = checked ? [...current, name] : current.filter((n) => n !== name);
    setSelected(next);
    void enqueue(async () => {
      try {
        if (isIdle(statusRef.current.phase)) {
          const base = settingsRef.current;
          if (base) setSettings(await saveSettings({ ...base, selectedApps: next }));
        } else {
          setSettings(await updateApps(next));
        }
      } catch (e) {
        setError(errorText(e));
      }
    });
  };

  const start = async () => {
    setBusy(true);
    try {
      if (!(await commit())) return;
      setSettings(await enqueue(() => startStream(selectedRef.current)));
      setError(null);
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  const stop = async () => {
    setBusy(true);
    try {
      await stopStream();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  // Selected apps that do not play now stay visible, so they can be unchecked.
  const absent = useMemo(() => {
    const listed = new Set((apps ?? []).map((app) => app.name));
    return selected.filter((name) => !listed.has(name));
  }, [apps, selected]);

  const idle = isIdle(status.phase);
  const editable = idle && draft !== null;
  const roomSet = settings !== null && settings.roomId !== "" && settings.username !== "";
  // Before the first sync the server offset can be unknown: a start would mute the apps
  // without a local copy. The status line says "Not connected" or "Syncing…" meanwhile.
  const canStart = idle && !busy && roomSet && appsError === null && selected.length > 0 && status.syncedOnce;

  return (
    <main className="app">
      <section className="fields">
        <label className="field">
          <span>Server</span>
          <input
            value={draft?.serverUrl ?? ""}
            disabled={!editable}
            spellCheck={false}
            onChange={(e) => edit("serverUrl", e.target.value)}
            onBlur={() => void commit()}
            onKeyDown={commitOnEnter}
          />
        </label>
        <div className="row">
          <label className="field">
            <span>Room</span>
            <input
              value={draft?.roomId ?? ""}
              disabled={!editable}
              inputMode="numeric"
              placeholder="123456"
              onChange={(e) => edit("roomId", e.target.value)}
              onBlur={() => void commit()}
              onKeyDown={commitOnEnter}
            />
          </label>
          <label className="field">
            <span>Name</span>
            <input
              value={draft?.username ?? ""}
              disabled={!editable}
              placeholder="Laptop"
              onChange={(e) => edit("username", e.target.value)}
              onBlur={() => void commit()}
              onKeyDown={commitOnEnter}
            />
          </label>
        </div>
        <div className="row">
          <label className="field">
            <span>Delay (ms)</span>
            <input
              type="number"
              min={MIN_DELAY_MS}
              max={MAX_DELAY_MS}
              step={50}
              value={draft?.delayMs ?? ""}
              disabled={!editable}
              onChange={(e) => edit("delayMs", e.target.value)}
              onBlur={() => void commit()}
              onKeyDown={commitOnEnter}
            />
          </label>
          <label className="field">
            <span>Local offset (ms)</span>
            <input
              type="number"
              step={1}
              value={draft?.localOffsetMs ?? ""}
              disabled={!editable}
              onChange={(e) => edit("localOffsetMs", e.target.value)}
              onBlur={() => void commit()}
              onKeyDown={commitOnEnter}
            />
            <span>negative = earlier</span>
          </label>
        </div>
      </section>

      <section className="apps">
        <div className="apps-head">
          <h2>Apps to stream</h2>
          <button type="button" disabled={loadingApps} onClick={() => void refreshApps()}>
            Refresh
          </button>
        </div>
        {appsError !== null ? (
          <p className="error">{appsError}</p>
        ) : (
          <ul className="app-list">
            {apps === null && <li className="hint">Looking for apps…</li>}
            {apps?.length === 0 && absent.length === 0 && <li className="hint">No app plays audio now.</li>}
            {apps?.map((app) => {
              const checked = selected.includes(app.name);
              return (
                <li key={app.name}>
                  <label className="app-row">
                    <input type="checkbox" checked={checked} onChange={(e) => toggle(app.name, e.target.checked)} />
                    <span className="app-name">{app.name}</span>
                  </label>
                  {app.isBrowser && checked && <p className="warning">{BROWSER_WARNING}</p>}
                </li>
              );
            })}
            {absent.map((name) => (
              <li key={name}>
                <label className="app-row">
                  <input type="checkbox" checked onChange={() => toggle(name, false)} />
                  <span className="app-name">{name}</span>
                  <span className="hint">not playing</span>
                </label>
              </li>
            ))}
          </ul>
        )}
      </section>

      <section className="status">
        <span>
          {status.connected ? (
            status.synced ? (
              <>
                Synced <span className="dot">●</span>
              </>
            ) : (
              "Syncing…"
            )
          ) : (
            "Not connected"
          )}
        </span>
        <span>RTT {status.rttMs === null ? "–" : Math.round(status.rttMs)} ms</span>
        <span>Delay {settings?.delayMs ?? "–"} ms</span>
      </section>

      {status.message !== null && <p className="error">{status.message}</p>}
      {error !== null && <p className="error">{error}</p>}

      <section className="actions">
        {idle ? (
          <button type="button" className="primary" disabled={!canStart} onClick={() => void start()}>
            Start stream
          </button>
        ) : (
          <button type="button" className="primary" disabled={busy} onClick={() => void stop()}>
            Stop stream
          </button>
        )}
        {status.phase === "starting" && <span className="phase">Starting…</span>}
        {isStreaming(status.phase) && <span className="phase live">LIVE</span>}
        <span className="listeners">Listeners: {status.listeners}</span>
      </section>
    </main>
  );
}
