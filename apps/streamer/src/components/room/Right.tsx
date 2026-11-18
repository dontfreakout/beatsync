import { MessageCircle, Radio } from "lucide-react";

import { Chat } from "@/components/room/Chat";
import { StreamTab } from "@/components/room/StreamTab";
import { Separator } from "@/components/ui/separator";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";

/** The web Right column with the tabs Chat and Stream; Spatial comes in part 2. */
export function Right() {
  return (
    <div className="flex h-full w-80 flex-shrink-0 flex-col border-l border-neutral-800/50 bg-neutral-900/50 backdrop-blur-md">
      <Tabs defaultValue="chat" className="flex h-full min-h-0 flex-col">
        <div className="flex-shrink-0 p-2 pb-0">
          <TabsList className="w-full bg-neutral-900">
            <TabsTrigger value="chat" className="flex-1">
              <MessageCircle className="mr-1.5 h-3.5 w-3.5" />
              Chat
            </TabsTrigger>
            <TabsTrigger value="stream" className="flex-1">
              <Radio className="mr-1.5 h-3.5 w-3.5" />
              Stream
            </TabsTrigger>
          </TabsList>
        </div>
        <Separator className="bg-neutral-800/50" />
        <TabsContent value="chat" className="h-full flex-1 overflow-hidden">
          <Chat />
        </TabsContent>
        <TabsContent value="stream" className="h-full flex-1 overflow-hidden">
          <StreamTab />
        </TabsContent>
      </Tabs>
    </div>
  );
}
