// The music search: the query, the loaded pages and the request state. The reply arrives as a
// `SEARCH_RESPONSE` room message (routed here by `App.tsx`).
import { type TrackType, WSResponseSchema } from "@beatsync/shared";
import { create } from "zustand";

import { sendRoomAction } from "@/lib/actions";

interface SearchStore {
  query: string;
  items: TrackType[];
  total: number;
  searching: boolean;
  loadingMore: boolean;
  error: string | null;
  submit: (query: string) => Promise<void>;
  /** Asks for the next page; the offset is the number of items loaded (the web's offset skips pages). */
  loadMore: () => Promise<void>;
  onResponse: (raw: unknown) => void;
  reset: () => void;
}

const initial = { query: "", items: [], total: 0, searching: false, loadingMore: false, error: null };

export const useSearchStore = create<SearchStore>()((set, get) => ({
  ...initial,
  submit: async (raw) => {
    const query = raw.trim();
    if (!query) return;
    set({ query, items: [], total: 0, searching: true, loadingMore: false, error: null });
    if (!(await sendRoomAction({ type: "SEARCH_MUSIC", query }))) set({ searching: false });
  },
  loadMore: async () => {
    const { query, items, total, searching, loadingMore } = get();
    if (!query || searching || loadingMore || items.length >= total) return;
    set({ loadingMore: true });
    if (!(await sendRoomAction({ type: "SEARCH_MUSIC", query, offset: items.length }))) set({ loadingMore: false });
  },
  onResponse: (raw) => {
    const parsed = WSResponseSchema.safeParse(raw);
    if (!parsed.success || parsed.data.type !== "SEARCH_RESPONSE") return;
    const result = parsed.data.response;
    if (result.type === "error") {
      set({ error: result.message, searching: false, loadingMore: false });
      return;
    }
    const { items, total } = result.response.data.tracks;
    set((s) => ({
      items: s.loadingMore ? [...s.items, ...items] : items,
      total,
      error: null,
      searching: false,
      loadingMore: false,
    }));
  },
  reset: () => set(initial),
}));
