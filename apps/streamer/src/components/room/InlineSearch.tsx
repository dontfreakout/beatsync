import { ArrowDown, Search as SearchIcon, ZapIcon } from "lucide-react";
import { AnimatePresence, motion } from "motion/react";
import { type FocusEvent, type FormEvent, useEffect, useRef, useState } from "react";

import { SearchResults } from "@/components/room/SearchResults";
import { cn } from "@/lib/utils";
import { canMutate as canMutateIn, useRoomStore } from "@/store/room";
import { useSearchStore } from "@/store/search";
import { useSessionStore } from "@/store/session";

const CHECKMARK_MS = 2000;

/** Port of the web `InlineSearch`: the search box with its results dropdown. */
export function InlineSearch() {
  const clientId = useSessionStore((s) => s.settings.clientId);
  const canMutate = useRoomStore((s) => canMutateIn(s, clientId));
  const activeStreamJobs = useRoomStore((s) => s.streamJobs);
  const hasSearched = useSearchStore((s) => s.searching || s.query !== "");
  const submit = useSearchStore((s) => s.submit);

  const [text, setText] = useState("");
  const [showResults, setShowResults] = useState(false);
  const [focused, setFocused] = useState(false);
  const [showCheckmark, setShowCheckmark] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);
  const checkmarkTimeout = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(
    () => () => {
      if (checkmarkTimeout.current) clearTimeout(checkmarkTimeout.current);
    },
    []
  );

  // Ctrl/Cmd+K toggles the focus (it also works from another input, such as the chat box, where
  // it moves the focus here); Esc blurs while the results show.
  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        if (showResults) {
          e.preventDefault();
          (document.activeElement as HTMLElement | null)?.blur();
        }
        return;
      }
      if (e.key === "k" && (e.metaKey || e.ctrlKey)) {
        e.preventDefault();
        if (!canMutate) return;
        if (focused) {
          inputRef.current?.blur();
          setShowResults(false);
        } else {
          inputRef.current?.focus();
        }
      }
    };
    document.addEventListener("keydown", onKeyDown);
    return () => document.removeEventListener("keydown", onKeyDown);
  }, [focused, canMutate, showResults]);

  // The results hide when the input is emptied.
  useEffect(() => {
    if (text.trim() === "") setShowResults(false);
  }, [text]);

  const onSubmit = (e: FormEvent) => {
    e.preventDefault();
    if (!canMutate || !text.trim()) return;
    setShowResults(true);
    void submit(text);
  };

  const onTrackSelect = () => {
    setShowCheckmark(true);
    setShowResults(false);
    setText("");
    if (checkmarkTimeout.current) clearTimeout(checkmarkTimeout.current);
    checkmarkTimeout.current = setTimeout(() => setShowCheckmark(false), CHECKMARK_MS);
  };

  // Focus that leaves the whole container hides the results.
  const onContainerBlur = (e: FocusEvent<HTMLDivElement>) => {
    if (!e.currentTarget.contains(e.relatedTarget)) setShowResults(false);
  };

  return (
    <div className="relative w-full" onBlur={onContainerBlur}>
      <form onSubmit={onSubmit}>
        <div className="group relative">
          <div className="absolute left-3 top-1/2 h-5 -translate-y-1/2 transform">
            <AnimatePresence mode="wait">
              {activeStreamJobs > 0 ? (
                <motion.div
                  key="streaming"
                  initial={{ opacity: 0, scale: 0.8 }}
                  animate={{ opacity: 1, scale: 1 }}
                  exit={{ opacity: 0, scale: 0.8 }}
                  transition={{ duration: 0.2, ease: "easeOut" }}
                  className="flex items-center gap-1.5"
                >
                  <div className="flex h-5 w-5 flex-shrink-0 items-center justify-center">
                    <svg className="h-full w-full" viewBox="0 0 100 100">
                      <motion.circle
                        cx="50"
                        cy="50"
                        r="35"
                        fill="none"
                        stroke="currentColor"
                        strokeWidth="8"
                        strokeLinecap="round"
                        className="text-green-500"
                        strokeDasharray={2 * Math.PI * 35 * 0.25}
                        animate={{ rotate: [0, 360] }}
                        transition={{ duration: 1.5, repeat: Infinity, ease: "linear" }}
                        style={{ transformOrigin: "center" }}
                      />
                    </svg>
                  </div>
                  <span className="font-mono text-xs font-medium text-green-400">{activeStreamJobs}</span>
                </motion.div>
              ) : showCheckmark ? (
                <motion.div
                  key="checkmark"
                  initial={{ scale: 0.8, opacity: 0 }}
                  animate={{ scale: 1, opacity: 1 }}
                  exit={{ scale: 0.8, opacity: 0 }}
                  transition={{ duration: 0.2, ease: "easeOut" }}
                  className="flex h-5 w-5 items-center justify-center"
                >
                  <ArrowDown className="h-full w-full text-green-500" />
                </motion.div>
              ) : (
                <motion.div
                  key="search"
                  initial={{ scale: 0.8, opacity: 0 }}
                  animate={{ scale: 1, opacity: 1 }}
                  exit={{ scale: 0.8, opacity: 0 }}
                  transition={{ duration: 0.2, ease: "easeOut" }}
                  className="flex h-5 w-5 items-center justify-center"
                >
                  <SearchIcon
                    className={cn(
                      "h-full w-full transition-colors duration-200",
                      canMutate ? "text-neutral-400 group-focus-within:text-white/80" : "text-neutral-600"
                    )}
                  />
                </motion.div>
              )}
            </AnimatePresence>
          </div>
          <input
            ref={inputRef}
            value={text}
            onChange={(e) => setText(e.target.value)}
            type="text"
            placeholder={canMutate ? "What do you want to play?" : "Search requires admin permissions"}
            onFocus={() => canMutate && setFocused(true)}
            onBlur={() => setFocused(false)}
            disabled={!canMutate}
            className={cn(
              "h-10 w-full truncate rounded-lg border border-neutral-600/30 pr-20 text-sm font-normal transition-all duration-200 focus:outline-none",
              activeStreamJobs > 0 ? "pl-14" : "pl-10",
              canMutate
                ? "bg-white/10 text-white placeholder:text-neutral-400 hover:bg-white/15 focus:bg-white/15 focus:ring-2 focus:ring-white/80"
                : "cursor-not-allowed bg-neutral-800/50 text-neutral-500 placeholder:text-neutral-600"
            )}
          />
          <div className="pointer-events-none absolute right-1 top-1/2 flex w-12 -translate-y-1/2 transform items-center justify-center">
            <AnimatePresence mode="wait">
              {showCheckmark ? (
                <motion.div
                  key="checkmark"
                  initial={{ scale: 0.8, opacity: 0 }}
                  animate={{ scale: 1, opacity: 1 }}
                  exit={{ scale: 0.8, opacity: 0 }}
                  transition={{ duration: 0.2, ease: "easeOut" }}
                  className="flex items-center justify-center"
                >
                  <ArrowDown className="h-5 w-5 text-green-500" />
                </motion.div>
              ) : (
                <motion.div
                  key="shortcut"
                  initial={{ scale: 0.8, opacity: 0 }}
                  animate={{ scale: 1, opacity: 1 }}
                  exit={{ scale: 0.8, opacity: 0 }}
                  transition={{ duration: 0.2, ease: "easeOut" }}
                  className="flex items-center justify-center"
                >
                  <kbd
                    className={cn(
                      "inline-flex h-6 items-center gap-0.5 rounded border border-neutral-600/50 bg-neutral-700/50 px-2 font-mono text-xs font-medium transition-colors duration-200",
                      canMutate ? "text-neutral-400" : "text-neutral-600 opacity-50"
                    )}
                  >
                    <span className="text-xs">⌘</span>K
                  </kbd>
                </motion.div>
              )}
            </AnimatePresence>
          </div>
        </div>
      </form>

      <div className="ml-0.5 mt-2 flex items-center gap-1 font-mono text-[10px] text-neutral-500">
        <ZapIcon className="size-3 stroke-1 text-neutral-400" />
        <span>[EXPERIMENTAL FREE BETA]</span>
      </div>

      <AnimatePresence>
        {showResults && canMutate && (
          <motion.div
            initial={{ opacity: 0, y: -10 }}
            animate={{ opacity: 1, y: 0 }}
            exit={{ opacity: 0, y: -10 }}
            transition={{ duration: 0.2, ease: "easeOut" }}
            className="absolute top-full z-[60] mt-2 w-full overflow-hidden rounded-2xl border border-neutral-700/50 bg-neutral-900/95 shadow-2xl backdrop-blur-xl"
          >
            <div className="scrollbar-thin max-h-[60vh] overflow-y-auto bg-neutral-900">
              {hasSearched ? (
                <SearchResults className="p-2" onTrackSelect={onTrackSelect} />
              ) : (
                <div className="p-8 text-center">
                  <h3 className="mb-2 text-lg font-medium text-white">Start typing to search</h3>
                  <p className="text-sm text-neutral-400">Find songs, artists, and albums</p>
                </div>
              )}
            </div>
          </motion.div>
        )}
      </AnimatePresence>
    </div>
  );
}
