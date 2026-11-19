import { MessageCircle, Radio, Rotate3D } from "lucide-react";

import { Chat } from "@/components/room/Chat";
import { SpatialTab } from "@/components/room/SpatialTab";
import { StreamTab } from "@/components/room/StreamTab";
import { Separator } from "@/components/ui/separator";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";

/** The web Right column with the tabs Chat, Spatial and Stream. */
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
            <TabsTrigger value="spatial" className="flex-1">
              <Rotate3D className="mr-1.5 h-3.5 w-3.5" />
              Spatial
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
        <TabsContent value="spatial" className="h-full flex-1 overflow-hidden">
          <SpatialTab />
        </TabsContent>
        <TabsContent value="stream" className="h-full flex-1 overflow-hidden">
          <StreamTab />
        </TabsContent>
      </Tabs>
    </div>
  );
}
