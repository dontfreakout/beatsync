import { useState } from "react";
import { Settings as SettingsIcon } from "lucide-react";

import { errorText, saveSettings } from "@/api";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { effectiveWebUrl } from "@/lib/urls";
import { useSessionStore } from "@/store/session";

/** Server and web addresses. They apply at the next join, so only the join screen shows this. */
export function SettingsDialog() {
  const settings = useSessionStore((s) => s.settings);
  const setSettings = useSessionStore((s) => s.setSettings);
  const [open, setOpen] = useState(false);
  const [serverUrl, setServerUrl] = useState("");
  const [webUrl, setWebUrl] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  const onOpenChange = (next: boolean) => {
    if (next) {
      setServerUrl(settings.serverUrl);
      setWebUrl(settings.webUrl);
      setError(null);
    }
    setOpen(next);
  };

  const save = async () => {
    if (webUrl.trim() && effectiveWebUrl("", webUrl) === null) {
      setError("The web address must start with http:// or https://");
      return;
    }
    setSaving(true);
    setError(null);
    try {
      setSettings(await saveSettings({ ...settings, serverUrl: serverUrl.trim(), webUrl: webUrl.trim() }));
      setOpen(false);
    } catch (e) {
      setError(errorText(e));
    } finally {
      setSaving(false);
    }
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogTrigger asChild>
        <Button variant="ghost" size="icon" aria-label="Settings" className="text-neutral-500 hover:text-neutral-300">
          <SettingsIcon className="size-4" />
        </Button>
      </DialogTrigger>
      <DialogContent className="bg-neutral-900 border-neutral-800">
        <DialogHeader>
          <DialogTitle>Settings</DialogTitle>
          <DialogDescription>The addresses apply at the next join.</DialogDescription>
        </DialogHeader>
        <div className="flex flex-col gap-4">
          <div className="flex flex-col gap-2">
            <Label htmlFor="server-url">Server address</Label>
            <Input
              id="server-url"
              value={serverUrl}
              onChange={(e) => setServerUrl(e.target.value)}
              placeholder="ws://host:8080/ws"
              spellCheck={false}
            />
          </div>
          <div className="flex flex-col gap-2">
            <Label htmlFor="web-url">Web address</Label>
            <Input
              id="web-url"
              value={webUrl}
              onChange={(e) => setWebUrl(e.target.value)}
              placeholder={effectiveWebUrl(serverUrl, "") ?? "http://host:3000"}
              spellCheck={false}
            />
          </div>
          {error && <p className="text-xs text-red-500">{error}</p>}
        </div>
        <DialogFooter>
          <Button onClick={() => void save()} disabled={saving}>
            Save
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
