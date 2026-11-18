import { Check, Copy, Hash, Link, QrCode } from "lucide-react";
import { AnimatePresence, motion } from "motion/react";
import QRCodeLib from "qrcode";
import { useEffect, useState } from "react";
import { toast } from "sonner";

import { errorText } from "@/api";
import { Dialog, DialogContent, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Separator } from "@/components/ui/separator";
import { cn } from "@/lib/utils";
import { effectiveWebUrl, hostOf, roomLink } from "@/lib/urls";
import { useSessionStore } from "@/store/session";

export function RoomHeader() {
  const settings = useSessionStore((s) => s.settings);
  const [open, setOpen] = useState(false);
  const [copied, setCopied] = useState(false);
  const [qrDataUrl, setQrDataUrl] = useState("");

  const webBase = effectiveWebUrl(settings.serverUrl, settings.webUrl);
  const roomUrl = webBase === null ? null : roomLink(webBase, settings.roomId);

  // The QR code is generated here; nothing is fetched.
  useEffect(() => {
    if (!open || roomUrl === null) return;
    let cancelled = false;
    QRCodeLib.toDataURL(roomUrl, {
      margin: 0,
      color: { dark: "#ffffff", light: "#00000000" },
      errorCorrectionLevel: "M",
      scale: 40,
    })
      .then((url) => {
        if (!cancelled) setQrDataUrl(url);
      })
      .catch((e) => console.error("QR code failed", e));
    return () => {
      cancelled = true;
    };
  }, [open, roomUrl]);

  const copy = async () => {
    if (roomUrl === null) return;
    try {
      await navigator.clipboard.writeText(roomUrl);
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    } catch (e) {
      toast.error(errorText(e));
    }
  };

  return (
    <>
      <motion.div className="mt-1 space-y-2.5 px-3.5 py-2">
        <div className="flex items-center justify-between">
          <div className="flex items-center gap-2 font-medium">
            <Hash size={18} />
            <span>Room {settings.roomId}</span>
          </div>
          <button
            className={cn(
              "flex cursor-pointer items-center gap-1 rounded-sm text-neutral-400 outline-none transition-colors hover:text-white focus-visible:ring-2 focus-visible:ring-neutral-400 focus-visible:ring-offset-2 focus-visible:ring-offset-neutral-900"
            )}
            onClick={() => setOpen(true)}
            type="button"
            aria-label="Show room QR code"
          >
            <QrCode size={16} />
            <span className="text-sm font-medium">QR</span>
          </button>
        </div>
      </motion.div>
      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent className="rounded-xl border border-neutral-800/60 bg-neutral-900/80 shadow-xl backdrop-blur-md transition-all duration-200">
          <DialogHeader>
            <DialogTitle className="flex items-center gap-2 text-base font-medium">
              <QrCode size={18} className="text-neutral-400" />
              Share Beatsync Room
            </DialogTitle>
          </DialogHeader>
          <Separator className="my-0 bg-neutral-800/50" />
          {roomUrl === null ? (
            <p className="pb-6 text-center text-sm text-neutral-400">Set the web address in the settings.</p>
          ) : (
            <div className="flex flex-col items-center space-y-6 pb-6">
              <div className="w-full space-y-2 text-center">
                <div className="text-3xl font-semibold text-white">{hostOf(roomUrl)}</div>
                <div className="inline-block rounded-lg border border-neutral-700/50 bg-neutral-800/50 px-8 py-2 text-3xl font-bold text-white">
                  {settings.roomId}
                </div>
              </div>

              <div className="relative">
                <div className="mb-2 text-center text-xs text-neutral-400">OR SCAN</div>
                <div className="w-full lg:px-8">
                  {qrDataUrl && <img src={qrDataUrl} alt="Room QR Code" className="h-full w-full" />}
                </div>
              </div>

              <button
                onClick={() => void copy()}
                className="group flex w-full cursor-pointer items-center justify-between rounded-lg border border-neutral-700/50 bg-neutral-800/50 px-4 py-3 transition-all duration-200 hover:bg-neutral-800/70"
              >
                <div className="flex items-center gap-3">
                  <div className="text-neutral-400">
                    <Link size={16} />
                  </div>
                  <div className="flex flex-col items-start">
                    <span className="text-xs uppercase tracking-wide text-neutral-500">Full URL</span>
                    <span className="max-w-[50vw] truncate font-mono text-sm text-white">{roomUrl}</span>
                  </div>
                </div>
                <div className="relative flex h-4 w-4 items-center justify-center">
                  <AnimatePresence mode="wait">
                    {copied ? (
                      <motion.div
                        key="check"
                        initial={{ scale: 0.8, opacity: 0 }}
                        animate={{ scale: 1, opacity: 1 }}
                        exit={{ scale: 0.8, opacity: 0 }}
                        transition={{ duration: 0.1, ease: "easeOut" }}
                        className="absolute inset-0 flex items-center justify-center"
                      >
                        <Check size={16} className="text-green-500" />
                      </motion.div>
                    ) : (
                      <motion.div
                        key="copy"
                        initial={{ scale: 0.8, opacity: 0 }}
                        animate={{ scale: 1, opacity: 1 }}
                        exit={{ scale: 0.8, opacity: 0 }}
                        transition={{ duration: 0.1, ease: "easeOut" }}
                        className="absolute inset-0 flex items-center justify-center text-neutral-400 transition-colors group-hover:text-white"
                      >
                        <Copy size={16} />
                      </motion.div>
                    )}
                  </AnimatePresence>
                </div>
              </button>
            </div>
          )}
        </DialogContent>
      </Dialog>
    </>
  );
}
