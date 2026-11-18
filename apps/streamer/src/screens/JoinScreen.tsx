import { useState } from "react";
import { PlusCircle } from "lucide-react";

import { errorText, joinRoom } from "@/api";
import { SettingsDialog } from "@/components/SettingsDialog";
import { Button } from "@/components/ui/button";
import { InputOTP, InputOTPGroup, InputOTPSlot } from "@/components/ui/input-otp";
import { generateName } from "@/lib/randomNames";
import { isRoomCode, randomRoomCode } from "@/lib/room";
import { useRoomStore } from "@/store/room";
import { useSessionStore } from "@/store/session";

export function JoinScreen() {
  const settings = useSessionStore((s) => s.settings);
  const setSettings = useSessionStore((s) => s.setSettings);
  const setJoined = useSessionStore((s) => s.setJoined);
  const [roomId, setRoomId] = useState(settings.roomId);
  const [username, setUsername] = useState(() => settings.username || generateName());
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const join = async (code: string) => {
    setBusy(true);
    setError(null);
    // The Rust hub sends no reset marker: clear the old room before the new one's messages arrive.
    useRoomStore.getState().reset();
    try {
      setSettings(await joinRoom(code, username.trim()));
      setJoined(true);
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  const canJoin = isRoomCode(roomId) && username.trim() !== "" && !busy;

  return (
    <div className="relative flex h-screen items-start justify-center bg-neutral-950 px-2.5 pt-24">
      <div className="absolute right-3 top-3">
        <SettingsDialog />
      </div>
      <div className="w-full max-w-[28rem]">
        <div className="flex flex-col items-center justify-center rounded-lg border border-neutral-800 bg-neutral-900 p-6 shadow-xl">
          <h2 className="mb-1 text-base font-medium tracking-tight text-white">Join a Beatsync Room</h2>
          <p className="mb-5 text-center text-xs text-neutral-400">Enter a room code to join or create a new room</p>

          <form
            className="w-full"
            onSubmit={(e) => {
              e.preventDefault();
              if (canJoin) void join(roomId);
            }}
          >
            <div className="flex justify-center">
              <InputOTP
                autoFocus
                maxLength={6}
                inputMode="numeric"
                autoComplete="one-time-code"
                data-lpignore="true"
                data-1p-ignore
                data-form-type="other"
                value={roomId}
                onChange={(value) => {
                  if (/^\d*$/.test(value)) setRoomId(value);
                }}
                className="gap-2"
              >
                <InputOTPGroup className="gap-2">
                  {Array.from({ length: 6 }).map((_, index) => (
                    <InputOTPSlot
                      key={index}
                      index={index}
                      className="h-10 w-9 border-neutral-700 bg-neutral-800/80 text-base transition-all duration-200 focus-within:border-primary/70 focus-within:bg-neutral-800 focus-within:ring-1 focus-within:ring-primary/30"
                    />
                  ))}
                </InputOTPGroup>
              </InputOTP>
            </div>

            <div className="mt-5 flex items-center justify-center">
              <div className="text-sm text-neutral-400">
                You&apos;ll join as <span className="inline-block font-medium text-primary">{username}</span>
              </div>
              <Button
                type="button"
                variant="ghost"
                className="ml-2 h-6 px-2 text-xs text-neutral-500 hover:text-neutral-300"
                disabled={busy}
                onClick={() => setUsername(generateName())}
              >
                Regenerate
              </Button>
            </div>

            <div className="mt-5 flex flex-col gap-3">
              <Button type="submit" className="w-full rounded-full" disabled={!canJoin}>
                {busy ? "Joining..." : "Join"}
              </Button>
              <button
                type="button"
                className="flex w-full cursor-pointer items-center justify-center rounded-full bg-neutral-800 px-5 py-2 text-sm font-medium tracking-wide text-white transition-colors hover:bg-neutral-700 disabled:opacity-50"
                disabled={busy || username.trim() === ""}
                onClick={() => {
                  const code = randomRoomCode();
                  setRoomId(code);
                  void join(code);
                }}
              >
                <PlusCircle size={16} className="mr-2" />
                <span>Create new room</span>
              </button>
            </div>
            {error && <p className="mt-3 text-center text-xs text-red-500">{error}</p>}
          </form>

          <p className="mt-5 text-center text-xs leading-relaxed text-neutral-500">Use native device speakers.</p>
        </div>
      </div>
    </div>
  );
}
