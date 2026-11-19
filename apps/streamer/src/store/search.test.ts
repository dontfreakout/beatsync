import { beforeEach, describe, expect, it, mock } from "bun:test";

const sent: unknown[] = [];
mock.module("@/lib/actions", () => ({
  sendRoomAction: (action: unknown) => {
    sent.push(action);
    return Promise.resolve(true);
  },
}));

const { useSearchStore } = await import("./search");

const track = (id: number) => ({
  id,
  title: `t${id}`,
  duration: 10,
  parental_warning: false,
  performer: { name: "p", id: 1 },
  track_number: 1,
  album: {
    image: { small: "", thumbnail: "", large: "" },
    title: "a",
    duration: 1,
    parental_warning: false,
    id: "al",
    release_date_original: "2020-01-01",
  },
});
const page = (ids: number[], total: number, offset = 0) => ({
  type: "SEARCH_RESPONSE",
  response: { type: "success", response: { data: { tracks: { limit: 50, offset, total, items: ids.map(track) } } } },
});

beforeEach(() => {
  sent.length = 0;
  useSearchStore.getState().reset();
});

describe("search store", () => {
  it("search_pages_append_and_offset_is_the_item_count", async () => {
    const s = useSearchStore.getState;
    await s().submit("  daft  ");
    expect(sent).toEqual([{ type: "SEARCH_MUSIC", query: "daft" }]);
    expect(s().searching).toBe(true);
    s().onResponse(page([1, 2, 3], 7));
    expect(s().items.map((t) => t.id)).toEqual([1, 2, 3]);
    expect(s().total).toBe(7);
    expect(s().searching).toBe(false);

    await s().loadMore();
    expect(sent[1]).toEqual({ type: "SEARCH_MUSIC", query: "daft", offset: 3 });
    expect(s().loadingMore).toBe(true);
    s().onResponse(page([4, 5], 7, 3));
    expect(s().items.map((t) => t.id)).toEqual([1, 2, 3, 4, 5]);
    expect(s().loadingMore).toBe(false);

    // A new search replaces.
    await s().submit("other");
    s().onResponse(page([9], 1));
    expect(s().items.map((t) => t.id)).toEqual([9]);
  });

  it("an_empty_query_or_a_full_list_sends_nothing", async () => {
    const s = useSearchStore.getState;
    await s().submit("   ");
    await s().loadMore();
    expect(sent).toEqual([]);
    await s().submit("a");
    s().onResponse(page([1], 1));
    await s().loadMore();
    expect(sent.length).toBe(1);
  });

  it("an_error_response_sets_the_message_and_stops_the_spinners", async () => {
    const s = useSearchStore.getState;
    await s().submit("a");
    s().onResponse({ type: "SEARCH_RESPONSE", response: { type: "error", message: "boom" } });
    expect(s().error).toBe("boom");
    expect(s().searching).toBe(false);
    await s().submit("b");
    expect(s().error).toBeNull();
  });

  it("a_bad_response_changes_nothing", async () => {
    const s = useSearchStore.getState;
    await s().submit("a");
    s().onResponse({ nonsense: true });
    expect(s().searching).toBe(true);
  });
});
