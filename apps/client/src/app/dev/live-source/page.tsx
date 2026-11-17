import { DevLiveSource } from "@/components/dev/DevLiveSource";
import { notFound } from "next/navigation";

export default function DevLiveSourcePage() {
  if (process.env.NODE_ENV !== "development") notFound();
  return <DevLiveSource />;
}
