import { motion } from "motion/react";
import { useEffect, useRef, useState } from "react";

import { sendRoomAction } from "@/lib/actions";
import { cn } from "@/lib/utils";
import { useRoomStore } from "@/store/room";

const LOADING_TIMEOUT_MS = 6000;

/** Port of the web `LoadDefaultTracksButton`: a spinner until the queue is non-empty or 6 s. */
export function LoadDefaultTracksButton({ className }: { className?: string }) {
  const queueLength = useRoomStore((s) => s.audioSources.length);
  const [loading, setLoading] = useState(false);
  const timeout = useRef<ReturnType<typeof setTimeout> | null>(null);

  const stop = () => {
    if (timeout.current) clearTimeout(timeout.current);
    timeout.current = null;
    setLoading(false);
  };

  useEffect(() => {
    if (loading && queueLength > 0) stop();
  }, [loading, queueLength]);

  useEffect(
    () => () => {
      if (timeout.current) clearTimeout(timeout.current);
    },
    []
  );

  const handleClick = async () => {
    if (loading) return;
    setLoading(true);
    if (timeout.current) clearTimeout(timeout.current);
    timeout.current = setTimeout(stop, LOADING_TIMEOUT_MS);
    // A failed send is toasted by `sendRoomAction`; the spinner must not wait for a reply then.
    if (!(await sendRoomAction({ type: "LOAD_DEFAULT_TRACKS" }))) stop();
  };

  return (
    <motion.button
      className={cn(
        "relative inline-flex cursor-pointer items-center justify-center rounded-full bg-primary px-5 py-2 text-xs font-medium tracking-wide text-primary-foreground",
        "transition-shadow duration-500 hover:shadow-lg hover:shadow-zinc-50/50",
        "disabled:cursor-not-allowed disabled:opacity-50",
        className
      )}
      whileHover={{ scale: 1.015 }}
      whileTap={{ scale: 0.985 }}
      transition={{ duration: 0.2 }}
      onClick={handleClick}
      disabled={loading}
    >
      {loading && (
        <div className="absolute inset-0 flex items-center justify-center">
          <div className="h-4 w-4">
            <svg className="h-full w-full" viewBox="0 0 100 100">
              <motion.circle
                cx="50"
                cy="50"
                r="35"
                fill="none"
                stroke="currentColor"
                strokeWidth="8"
                strokeLinecap="round"
                className="text-primary-foreground"
                strokeDasharray={2 * Math.PI * 35 * 0.25}
                animate={{ rotate: [0, 360] }}
                transition={{ duration: 1.5, repeat: Infinity, ease: "linear" }}
                style={{ transformOrigin: "center" }}
              />
            </svg>
          </div>
        </div>
      )}
      <span className={cn(loading ? "opacity-0" : "opacity-100")}>Load default tracks</span>
    </motion.button>
  );
}
