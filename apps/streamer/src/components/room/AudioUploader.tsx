import { open } from "@tauri-apps/plugin-dialog";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { CloudUpload, Plus } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { toast } from "sonner";

import { errorText, uploadAudio } from "@/api";
import { AUDIO_EXTENSIONS, isAudioPath, isInsideRect, uploadLabel } from "@/lib/upload";
import { cn } from "@/lib/utils";
import { canMutate as canMutateIn, useRoomStore } from "@/store/room";
import { useSessionStore } from "@/store/session";

/** Port of the web `AudioUploaderMinimal`. The native dialog and file drop give a path; the core uploads it. */
export function AudioUploader() {
  const clientId = useSessionStore((s) => s.settings.clientId);
  const canMutate = useRoomStore((s) => canMutateIn(s, clientId));
  const [isDragging, setIsDragging] = useState(false);
  const [isUploading, setIsUploading] = useState(false);
  const [fileName, setFileName] = useState<string | null>(null);
  const zoneRef = useRef<HTMLDivElement>(null);
  const clearTimer = useRef<ReturnType<typeof setTimeout>>(undefined);
  const busy = useRef(false);
  const allowed = useRef(canMutate);
  allowed.current = canMutate;

  const upload = useCallback(async (path: string) => {
    if (busy.current || !allowed.current) return;
    busy.current = true;
    clearTimeout(clearTimer.current);
    setFileName(uploadLabel(path));
    setIsUploading(true);
    try {
      await uploadAudio(path);
      clearTimer.current = setTimeout(() => setFileName(null), 3000);
    } catch (e) {
      toast.error("Failed to upload audio file", { description: errorText(e) });
      setFileName(null);
    } finally {
      busy.current = false;
      setIsUploading(false);
    }
  }, []);

  const pick = async () => {
    if (busy.current || !canMutate) return;
    try {
      const path = await open({ multiple: false, filters: [{ name: "Audio", extensions: AUDIO_EXTENSIONS }] });
      if (path) void upload(path);
    } catch (e) {
      // The uploader has not left idle yet: only the toast is left to do.
      toast.error("Failed to upload audio file", { description: errorText(e) });
    }
  };

  useEffect(() => {
    const overZone = (position: { x: number; y: number }) => {
      const rect = zoneRef.current?.getBoundingClientRect();
      return !!rect && isInsideRect(position, window.devicePixelRatio, rect);
    };
    const unlisten = getCurrentWebview().onDragDropEvent((event) => {
      const payload = event.payload;
      if (payload.type === "leave") return setIsDragging(false);
      const inside = allowed.current && overZone(payload.position);
      if (payload.type !== "drop") return setIsDragging(inside);
      setIsDragging(false);
      const path = payload.paths[0];
      if (!inside || !path) return;
      if (!isAudioPath(path)) return void toast.error("Please select an audio file");
      void upload(path);
    });
    // A failed listen means no file drop; the picker still works.
    const listening = unlisten.then((fn) => fn);
    listening.catch((e) => console.error("Cannot listen for file drops", e));
    return () => void listening.then((fn) => fn()).catch(() => undefined);
  }, [upload]);

  const isDisabled = !canMutate;

  return (
    <div
      ref={zoneRef}
      className={cn(
        "mx-2 overflow-hidden rounded-md border border-neutral-700/50 transition-all",
        isDisabled ? "bg-neutral-800/20 opacity-50" : "bg-neutral-800/30 hover:bg-neutral-800/50",
        isDragging && !isDisabled ? "outline-dashed outline-primary-400" : "outline-none",
      )}
      title={isDisabled ? "Admin-only mode - only admins can upload" : undefined}
    >
      <button
        type="button"
        className={cn("block w-full text-left", isDisabled ? "" : "cursor-pointer")}
        disabled={isUploading || isDisabled}
        onClick={() => void pick()}
      >
        <div className="flex items-center gap-3 p-3">
          <div
            className={cn(
              "flex-shrink-0 rounded-md p-1.5",
              isDisabled ? "bg-neutral-600 text-neutral-400" : "bg-primary-700 text-white",
            )}
          >
            {isUploading ? <CloudUpload className="h-4 w-4 animate-pulse" /> : <Plus className="h-4 w-4" />}
          </div>
          <div className="min-w-0 flex-1">
            <div className="truncate text-xs font-medium text-white">
              {isUploading ? "Uploading..." : fileName ? fileName : "Upload audio"}
            </div>
            {!isUploading && !fileName && (
              <div className={cn("truncate text-xs", isDisabled ? "text-neutral-500" : "text-neutral-400")}>
                {isDisabled ? "Must be an admin to upload" : "Add music to queue"}
              </div>
            )}
          </div>
        </div>
      </button>
    </div>
  );
}
