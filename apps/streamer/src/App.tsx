import { useEffect, useRef, useState } from "react";

import { getSession, subscribeRoom, subscribeStatus, subscribeTracks } from "@/api";
import { Toaster } from "@/components/ui/sonner";
import { JoinScreen } from "@/screens/JoinScreen";
import { RoomScreen } from "@/screens/RoomScreen";
import { SyncScreen } from "@/screens/SyncScreen";
import { useRoomStore } from "@/store/room";
import { useSearchStore } from "@/store/search";
import { useSessionStore } from "@/store/session";

export function App() {
  const joined = useSessionStore((s) => s.joined);
  const syncedOnce = useSessionStore((s) => s.status.syncedOnce);
  // The join form starts from the saved settings, so wait for them.
  const [ready, setReady] = useState(false);

  // Once, also under StrictMode: a second subscription would replace the first.
  const started = useRef(false);
  useEffect(() => {
    if (started.current) return;
    started.current = true;
    const { setStatus, setSession, setTracks } = useSessionStore.getState();
    const start = async () => {
      await subscribeStatus(setStatus);
      await subscribeTracks(setTracks);
      await subscribeRoom((raw) => {
        // The core forwards the search reply as a plain room message.
        if (typeof raw === "object" && raw !== null && "type" in raw && raw.type === "SEARCH_RESPONSE") {
          useSearchStore.getState().onResponse(raw);
        } else {
          useRoomStore.getState().apply(raw);
        }
      });
      setSession(await getSession());
    };
    start()
      .catch((e) => console.error("startup failed", e))
      .finally(() => setReady(true));
  }, []);

  return (
    <>
      {!ready ? null : !joined ? <JoinScreen /> : !syncedOnce ? <SyncScreen /> : <RoomScreen />}
      <Toaster />
    </>
  );
}
