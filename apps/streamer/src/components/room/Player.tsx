import { Pause, Play, Repeat, Shuffle, SkipBack, SkipForward } from "lucide-react";
import { useEffect, useRef, useState } from "react";

import { setShuffle } from "@/api";
import { Slider } from "@/components/ui/slider";
import { sendRoomAction } from "@/lib/actions";
import {
  formatTime,
  heldSeekPosition,
  isSelectedPlaying,
  nextTrack,
  playbackKey,
  previousTrack,
  seekAction,
  selectedPosition,
  sendHeldSeek,
  serverNowMs,
  skipAction,
} from "@/lib/player";
import { rowClickAction } from "@/lib/queue";
import { cn } from "@/lib/utils";
import { selectedUrl, usePlayerStore } from "@/store/player";
import { canControl as canControlIn, useRoomStore } from "@/store/room";
import { useSessionStore } from "@/store/session";

/** The selected track, its duration, and whether it plays, read from the stores at call time. */
function readPlayer() {
  const room = useRoomStore.getState();
  const { localSelection, pausedPosition } = usePlayerStore.getState();
  const session = useSessionStore.getState();
  const selected = selectedUrl(room, localSelection);
  const track = selected === null ? undefined : session.tracks[selected];
  const durationS = track?.state === "ready" ? track.durationS : null;
  const serverNow = serverNowMs(session.status.offsetMs);
  const playing = isSelectedPlaying(room.playback, selected, serverNow, durationS);
  const position = selectedPosition({ playback: room.playback, selected, pausedPosition, serverNow, durationS });
  return { room, selected, pausedPosition, playing, position, canControl: canControlIn(room, session.settings.clientId) };
}

/** Play or pause the selected track, as a click on its queue row. */
function togglePlay() {
  const { room, selected, pausedPosition, playing, position, canControl } = readPlayer();
  const url = selected ?? room.audioSources[0]?.url ?? null;
  if (!canControl || url === null) return;
  void sendRoomAction(rowClickAction({ url, selected: url, playing, position, pausedPosition }));
}

/** Web previous/next: PLAY 0 while playing, else only the local selection changes. */
function skipTo(url: string | null) {
  const { playing, canControl } = readPlayer();
  if (!canControl || url === null) return;
  const action = skipAction(url, playing);
  if (action) void sendRoomAction(action);
  else usePlayerStore.getState().setLocalSelection(url);
}

/** Port of the web `Player`: transport buttons, seek slider and times. */
export function Player() {
  const audioSources = useRoomStore((s) => s.audioSources);
  const clientId = useSessionStore((s) => s.settings.clientId);
  const canControl = useRoomStore((s) => canControlIn(s, clientId));
  const shuffled = useSessionStore((s) => s.status.shuffle);
  const playbackKeyNow = useRoomStore((s) => playbackKey(s.playback));
  const playbackKind = useRoomStore((s) => s.playback?.kind);
  // Subscribe, so that the render below reads fresh values.
  useRoomStore((s) => s.currentAudioSource);
  usePlayerStore((s) => s.localSelection);
  usePlayerStore((s) => s.pausedPosition);
  const tracks = useSessionStore((s) => s.tracks);
  useSessionStore((s) => s.status.offsetMs);

  // The progress refreshes every 250 ms while the room plays.
  const [, setTick] = useState(0);
  useEffect(() => {
    if (playbackKind !== "PLAY") return;
    const id = setInterval(() => setTick((n) => n + 1), 250);
    return () => clearInterval(id);
  }, [playbackKind]);

  const { selected, playing, position } = readPlayer();
  const track = selected === null ? undefined : tracks[selected];
  const duration = track?.state === "ready" ? track.durationS : 0;
  const urls = audioSources.map((source) => source.url);
  const manyTracks = urls.length > 1;

  // A drag shows its own value. `pendingSeek` also covers a commit that Radix swallows (web).
  const [dragPosition, setDragPosition] = useState<number | null>(null);
  const pendingSeek = useRef<number | null>(null);
  // After a commit the seek bar keeps the committed position until the server sets a new playback
  // (or the seek was not sent).
  const [heldSeek, setHeldSeek] = useState<{ position: number; key: string } | null>(null);
  if (heldSeek !== null && heldSeek.key !== playbackKeyNow) setHeldSeek(null);
  const commitSeek = () => {
    const value = pendingSeek.current;
    if (value === null) return;
    pendingSeek.current = null;
    setDragPosition(null);
    const now = readPlayer();
    if (!now.canControl || now.selected === null) return;
    const action = seekAction(now.selected, now.playing, value);
    if (action) {
      const hold = { position: value, key: playbackKey(now.room.playback) };
      void sendHeldSeek(hold, setHeldSeek, () => sendRoomAction(action));
    } else usePlayerStore.getState().setPausedPosition(value);
  };

  // Space toggles play/pause, except while typing. The listener exists only while the player does.
  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      const target = e.target as HTMLElement | null;
      if (
        e.code === "Space" &&
        !(target instanceof HTMLInputElement || target instanceof HTMLTextAreaElement || target?.isContentEditable)
      ) {
        e.preventDefault();
        togglePlay();
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, []);

  const shown = dragPosition ?? heldSeekPosition(heldSeek, playbackKeyNow) ?? position;
  const side = "cursor-pointer text-gray-400 transition-colors duration-200 hover:scale-105 hover:text-white disabled:cursor-not-allowed disabled:opacity-50";

  return (
    <div className="flex w-full justify-center">
      <div className="w-full max-w-[37rem]">
        <div className="mb-2 flex items-center justify-center gap-6">
          <button className={side} onClick={() => void setShuffle(!shuffled)} disabled={!manyTracks || !canControl}>
            <div className="relative">
              <Shuffle className={cn("relative size-4", shuffled ? "text-primary-400" : "text-current")} />
              {shuffled && (
                <div className="absolute bottom-0 left-1/2 top-4.5 h-1 w-1 -translate-x-1/2 translate-y-1/2 transform rounded-full bg-green-500" />
              )}
            </div>
          </button>
          <button
            className={side}
            onClick={() => skipTo(previousTrack(urls, selected))}
            disabled={shuffled || !manyTracks || !canControl}
          >
            <SkipBack className="h-5 w-5 fill-current" />
          </button>
          <button
            className="cursor-pointer rounded-full bg-white p-2 text-black transition-transform duration-200 hover:scale-105 focus:outline-none disabled:cursor-not-allowed disabled:opacity-50"
            onClick={togglePlay}
            disabled={!canControl}
          >
            {playing ? <Pause className="h-4 w-4 fill-current stroke-1" /> : <Play className="h-4 w-4 fill-current" />}
          </button>
          <button
            className={side}
            onClick={() => skipTo(nextTrack(urls, selected, shuffled))}
            disabled={!manyTracks || !canControl}
          >
            <SkipForward className="h-5 w-5 fill-current" />
          </button>
          <span className="cursor-default text-gray-400">
            <div className="relative">
              <Repeat className="text-primary-400 relative h-4 w-4" />
              <div className="absolute bottom-0 left-1/2 top-4.5 h-1 w-1 -translate-x-1/2 translate-y-1/2 transform rounded-full bg-green-500" />
            </div>
          </span>
        </div>
        <div className="flex items-center gap-0">
          <span className="min-w-11 select-none text-xs text-muted-foreground">{formatTime(shown)}</span>
          <Slider
            value={[shown]}
            min={0}
            max={duration || 1}
            step={0.01}
            onValueChange={([value]) => {
              if (!canControl) return;
              pendingSeek.current = value;
              setDragPosition(value);
            }}
            onValueCommit={commitSeek}
            onPointerUp={commitSeek}
            disabled={!canControl || duration <= 0}
            className={cn(!canControl && "opacity-50")}
          />
          <span className="min-w-11 select-none text-right text-xs text-muted-foreground">
            {duration > 0 ? formatTime(duration) : ""}
          </span>
        </div>
      </div>
    </div>
  );
}
