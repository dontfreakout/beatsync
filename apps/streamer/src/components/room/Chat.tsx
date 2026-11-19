import { CHAT_CONSTANTS } from "@beatsync/shared";
import { ChevronDown, MessageCircle } from "lucide-react";
import { AnimatePresence, motion } from "motion/react";
import { useCallback, useEffect, useRef, useState } from "react";

import { ScrollArea } from "@/components/ui/scroll-area";
import { sendRoomAction } from "@/lib/actions";
import { countryFlag, formatChatTimestamp, groupMessages } from "@/lib/chat";
import { cn } from "@/lib/utils";
import { useRoomStore } from "@/store/room";
import { useSessionStore } from "@/store/session";

const TIMESTAMP_GAP_THRESHOLD_MS = 60_000;
/** How far from the bottom still counts as "at the bottom". */
const AT_BOTTOM_THRESHOLD_PX = 300;

/** Port of the web client's chat (apps/client/src/components/dashboard/right/Chat.tsx). */
export function Chat() {
  const messages = useRoomStore((s) => s.chat.messages);
  const clientId = useSessionStore((s) => s.settings.clientId);
  const connected = useSessionStore((s) => s.status.connected);

  const [draft, setDraft] = useState("");
  const [composing, setComposing] = useState(false);
  const [inputAreaHeight, setInputAreaHeight] = useState(60);
  // The message count when the user scrolled away from the bottom; null while at the bottom.
  const [scrolledUpAt, setScrolledUpAt] = useState<number | null>(null);

  const scrollAreaRef = useRef<HTMLDivElement>(null);
  const inputAreaRef = useRef<HTMLDivElement>(null);
  const countRef = useRef(messages.length);
  countRef.current = messages.length;
  const previousCount = useRef(0);
  // A second Enter while the first send is on its way must not send the text twice.
  const sending = useRef(false);

  const newMessageCount = scrolledUpAt === null ? 0 : messages.length - scrolledUpAt;

  const scrollToBottom = useCallback((behavior: ScrollBehavior = "smooth") => {
    const viewport = scrollAreaRef.current?.querySelector("[data-radix-scroll-area-viewport]");
    if (!viewport) return;
    viewport.scrollTo({ top: viewport.scrollHeight, behavior });
    setScrolledUpAt(null);
  }, []);

  // Follow new messages while at the bottom; jump to the bottom on mount.
  useEffect(() => {
    if (messages.length > previousCount.current && scrolledUpAt === null) {
      scrollToBottom(previousCount.current === 0 ? "auto" : "smooth");
    }
    previousCount.current = messages.length;
  }, [messages.length, scrolledUpAt, scrollToBottom]);

  useEffect(() => {
    const viewport = scrollAreaRef.current?.querySelector("[data-radix-scroll-area-viewport]");
    if (!viewport) return;
    const onScroll = () => {
      const atBottom =
        Math.abs(viewport.scrollHeight - viewport.clientHeight - viewport.scrollTop) < AT_BOTTOM_THRESHOLD_PX;
      setScrolledUpAt((current) => (atBottom ? null : (current ?? countRef.current)));
    };
    viewport.addEventListener("scroll", onScroll);
    return () => viewport.removeEventListener("scroll", onScroll);
  }, []);

  // The messages need padding as high as the input area.
  useEffect(() => {
    const el = inputAreaRef.current;
    if (!el) return;
    const observer = new ResizeObserver(() => setInputAreaHeight(el.offsetHeight));
    observer.observe(el);
    return () => observer.disconnect();
  }, []);

  const send = async () => {
    const text = draft.trim();
    if (!text || composing || !connected || sending.current) return;
    sending.current = true;
    const sent = await sendRoomAction({ type: "SEND_CHAT_MESSAGE", text });
    sending.current = false;
    // On a failure the toast says why, and the text stays for another try.
    if (!sent) return;
    setDraft("");
    scrollToBottom("auto");
  };

  const groups = groupMessages(messages);

  return (
    <div className="relative h-full overflow-hidden">
      <div className="h-full" style={{ paddingBottom: `${inputAreaHeight}px` }}>
        <ScrollArea ref={scrollAreaRef} className="h-full px-2 pt-3">
          <AnimatePresence>
            {messages.length === 0 && (
              <motion.div
                initial={{ opacity: 0 }}
                animate={{ opacity: 1 }}
                exit={{ opacity: 0 }}
                transition={{ duration: 0.15, ease: "easeOut" }}
                className="absolute inset-0 flex flex-col items-center justify-center px-4"
              >
                <MessageCircle className="mb-3 h-12 w-12 text-neutral-700" />
                <h3 className="mb-1 text-sm font-medium text-neutral-400">No messages yet</h3>
                <p className="text-xs text-neutral-600">Start the conversation</p>
              </motion.div>
            )}
          </AnimatePresence>

          <div className="space-y-2 pb-2">
            {groups.map((group, groupIndex) => {
              const first = group[0];
              const isOwn = first.clientId === clientId;
              const previous = groups[groupIndex - 1];
              const showTimestamp =
                !previous || first.timestamp - previous[previous.length - 1].timestamp > TIMESTAMP_GAP_THRESHOLD_MS;
              const flag = first.countryCode ? countryFlag(first.countryCode) : "";

              return (
                <div key={`group-${first.id}`} className="space-y-0.5">
                  {showTimestamp && (
                    <div className="flex items-center justify-center py-1">
                      <span className="text-[10px] font-medium text-neutral-500">
                        {formatChatTimestamp(first.timestamp)}
                      </span>
                    </div>
                  )}

                  <div className={cn("flex w-full min-w-0 flex-col", isOwn ? "items-end" : "items-start")}>
                    {!isOwn && (
                      <span className="mb-0.5 ml-1 text-[10px] text-neutral-500">
                        <span title={first.countryCode ? `Country: ${first.countryCode}` : undefined}>
                          {flag && `${flag} `}
                          {first.username}
                          {first.isCreator && (
                            <span className="ml-0.5 rounded bg-sky-500/15 px-0.5 font-semibold text-sky-400">
                              Creator
                            </span>
                          )}
                        </span>
                      </span>
                    )}

                    <div className={cn("flex max-w-[85%] flex-col gap-[1px]", isOwn ? "items-end" : "items-start")}>
                      <AnimatePresence mode="popLayout">
                        {group.map((msg, index) => {
                          const isFirst = index === 0;
                          const isLast = index === group.length - 1;
                          return (
                            <motion.div
                              key={msg.id}
                              className={cn(
                                "px-3 py-1.5 text-sm",
                                msg.isCreator
                                  ? "bg-sky-700 text-white"
                                  : isOwn
                                    ? "bg-green-700 text-white"
                                    : "bg-neutral-800 text-neutral-200",
                                group.length === 1
                                  ? "rounded-2xl"
                                  : [
                                      isFirst && isOwn && "rounded-2xl rounded-br-md",
                                      isFirst && !isOwn && "rounded-2xl rounded-bl-md",
                                      isLast && isOwn && "rounded-2xl rounded-tr-md",
                                      isLast && !isOwn && "rounded-2xl rounded-tl-md",
                                      !isFirst && !isLast && isOwn && "rounded-l-2xl rounded-r-md",
                                      !isFirst && !isLast && !isOwn && "rounded-r-2xl rounded-l-md",
                                    ]
                              )}
                              initial={{ opacity: 0, y: 10, scale: 0.95 }}
                              animate={{ opacity: 1, y: 0, scale: 1 }}
                              exit={{ opacity: 0, scale: 0.95 }}
                              transition={{ type: "spring", stiffness: 700, damping: 35, mass: 0.3 }}
                              layout
                            >
                              <p className="whitespace-pre-wrap wrap-anywhere">{msg.text}</p>
                            </motion.div>
                          );
                        })}
                      </AnimatePresence>
                    </div>
                  </div>
                </div>
              );
            })}
          </div>
        </ScrollArea>
      </div>

      <AnimatePresence>
        {newMessageCount > 0 && (
          <motion.button
            initial={{ opacity: 0, y: 20, scale: 0.9 }}
            animate={{ opacity: 1, y: 0, scale: 1 }}
            exit={{ opacity: 0, y: 20, scale: 0.9 }}
            transition={{ type: "spring", stiffness: 500, damping: 30 }}
            onClick={() => scrollToBottom()}
            className="absolute left-1/2 z-20 flex -translate-x-1/2 items-center gap-1 rounded-full bg-green-800 px-3 py-1.5 text-xs font-medium text-white shadow-lg shadow-green-900/40 transition-colors duration-500 hover:bg-green-700"
            style={{ bottom: `${inputAreaHeight + 16}px` }}
          >
            <ChevronDown className="h-3 w-3" />
            {newMessageCount === 1 ? "1 new message" : `${newMessageCount} new messages`}
          </motion.button>
        )}
      </AnimatePresence>

      <div
        ref={inputAreaRef}
        className="absolute right-0 bottom-0 left-0 z-10 border-t border-neutral-800/50 bg-neutral-900 p-2 pt-3"
      >
        <textarea
          value={draft}
          disabled={!connected}
          maxLength={CHAT_CONSTANTS.MAX_MESSAGE_LENGTH}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey && !composing) {
              e.preventDefault();
              void send();
            }
          }}
          onCompositionStart={() => setComposing(true)}
          onCompositionEnd={() => setComposing(false)}
          placeholder={connected ? "Message" : "Not connected"}
          rows={1}
          className={cn(
            "w-full resize-none rounded-2xl border border-neutral-700/50 bg-neutral-800/50 px-4 py-2 text-sm",
            "text-neutral-100 placeholder:text-neutral-500 focus:outline-none",
            "field-sizing-content max-h-[120px] overflow-auto disabled:cursor-not-allowed disabled:opacity-50"
          )}
        />
      </div>
    </div>
  );
}
