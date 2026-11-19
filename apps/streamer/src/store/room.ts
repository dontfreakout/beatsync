// The room as the window knows it, rebuilt from the raw server messages that the Rust core
// forwards (`subscribe_room`). Each message is validated on its own with the shared schema: a
// bad one changes nothing, so it cannot block the rest of a snapshot.
import {
  type AudioSourceType,
  type ChatMessageType,
  type ClientDataType,
  type LiveStreamType,
  type PlaybackControlsPermissionsType,
  WSResponseSchema,
} from "@beatsync/shared";
import { create } from "zustand";

export interface PlaybackState {
  kind: "PLAY" | "PAUSE";
  serverTimeToExecute: number;
  audioSource?: string;
  trackTimeSeconds?: number;
}

export interface RoomState {
  clients: ClientDataType[];
  audioSources: AudioSourceType[];
  currentAudioSource: string | null;
  permissions: PlaybackControlsPermissionsType;
  liveStream: LiveStreamType | null;
  chat: { messages: ChatMessageType[]; newestId: number };
  playback: PlaybackState | null;
  globalVolume: number;
  lowPassFreq: number;
  metronome: boolean;
  spatial: {
    enabled: boolean;
    /** The own and other clients' gain, 0..1, by client id. */
    gains: Record<string, number>;
    listeningSource: { x: number; y: number };
  };
  /** Active music stream jobs (a search result being added to the queue). */
  streamJobs: number;
}

/** `ADMIN_ONLY` until the server says otherwise, the safe side for the UI. */
export const initialRoomState: RoomState = {
  clients: [],
  audioSources: [],
  currentAudioSource: null,
  permissions: "ADMIN_ONLY",
  liveStream: null,
  chat: { messages: [], newestId: 0 },
  playback: null,
  globalVolume: 1,
  lowPassFreq: 20000,
  metronome: false,
  spatial: { enabled: false, gains: {}, listeningSource: { x: 50, y: 50 } },
  streamJobs: 0,
};

/**
 * Applies one raw server message. A message that fails the schema, or a message the window does not
 * show, returns `state` itself.
 */
export function applyServerMessage(state: RoomState, raw: unknown): RoomState {
  const parsed = WSResponseSchema.safeParse(raw);
  if (!parsed.success) return state;
  const message = parsed.data;

  if (message.type === "ROOM_EVENT") {
    const event = message.event;
    switch (event.type) {
      case "CLIENT_CHANGE":
        return { ...state, clients: event.clients };
      case "SET_AUDIO_SOURCES": {
        // The web store's rule: the server omits `currentAudioSource` on reorder, delete and
        // default tracks. Keep the current source then, unless the queue no longer has it.
        const currentAudioSource =
          event.currentAudioSource ??
          (event.sources.some((source) => source.url === state.currentAudioSource) ? state.currentAudioSource : null);
        // A reorder keeps the playback. A removed current track drops it, as the server does.
        return {
          ...state,
          audioSources: event.sources,
          currentAudioSource,
          playback: currentAudioSource === null ? null : state.playback,
        };
      }
      case "LOAD_AUDIO_SOURCE":
        // The web selects the track the server is about to play.
        return { ...state, currentAudioSource: event.audioSourceToPlay.url };
      case "SET_PLAYBACK_CONTROLS":
        return { ...state, permissions: event.permissions };
      case "LIVE_STREAM_STATE":
        return { ...state, liveStream: event.stream };
      case "CHAT_UPDATE":
        // The web chat store's rule: a full sync replaces, an update appends the newer ids.
        if (event.isFullSync) return { ...state, chat: { messages: event.messages, newestId: event.newestId } };
        return {
          ...state,
          chat: {
            messages: [...state.chat.messages, ...event.messages.filter((m) => m.id > state.chat.newestId)],
            newestId: Math.max(state.chat.newestId, event.newestId),
          },
        };
      default:
        return state;
    }
  }

  if (message.type === "STREAM_JOB_UPDATE") return { ...state, streamJobs: message.activeJobCount };

  if (message.type === "SCHEDULED_ACTION") {
    const action = message.scheduledAction;
    switch (action.type) {
      case "GLOBAL_VOLUME_CONFIG":
        return { ...state, globalVolume: action.volume };
      case "LOW_PASS_CONFIG":
        return { ...state, lowPassFreq: action.freq };
      case "METRONOME_CONFIG":
        return { ...state, metronome: action.enabled };
      case "SPATIAL_CONFIG":
        return {
          ...state,
          spatial: {
            enabled: true,
            gains: Object.fromEntries(Object.entries(action.gains).map(([id, g]) => [id, g.gain])),
            listeningSource: action.listeningSource,
          },
        };
      case "STOP_SPATIAL_AUDIO":
        return { ...state, spatial: { ...state.spatial, enabled: false, gains: {} } };
    }
    if (action.type === "PLAY" || action.type === "PAUSE") {
      return {
        ...state,
        currentAudioSource: action.type === "PLAY" ? action.audioSource : state.currentAudioSource,
        playback: {
          kind: action.type,
          serverTimeToExecute: message.serverTimeToExecute,
          audioSource: action.audioSource,
          trackTimeSeconds: action.trackTimeSeconds,
        },
      };
    }
  }
  return state;
}

export const isAdmin = (state: RoomState, clientId: string): boolean =>
  state.clients.some((c) => c.clientId === clientId && c.isAdmin);

/** Whether this client may change the queue and playback: an admin, or anyone with `EVERYONE`. */
export const canMutate = (state: RoomState, clientId: string): boolean =>
  state.permissions === "EVERYONE" || isAdmin(state, clientId);

/**
 * Whether the controls (queue, player, search, upload) work for this client. A running live
 * stream replaces the player, as on the web.
 */
export const canControl = (state: RoomState, clientId: string): boolean =>
  canMutate(state, clientId) && state.liveStream === null;

interface RoomStore extends RoomState {
  apply: (raw: unknown) => void;
  /** Call before `join_room` and after `leave_room`: the Rust hub sends no reset marker. */
  reset: () => void;
}

export const useRoomStore = create<RoomStore>()((set) => ({
  ...initialRoomState,
  // An ignored message returns the same state object, so zustand does not notify.
  apply: (raw) => set((store) => applyServerMessage(store, raw)),
  reset: () => set(initialRoomState),
}));
