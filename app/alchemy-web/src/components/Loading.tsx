import { LoaderCircle } from "lucide-react";

/** A spinner and a line saying what is loading, announced politely. */
export function Loading({ label = "Loading…" }: { label?: string }) {
  return (
    <p role="status" className="flex items-center gap-2 py-4 text-muted">
      <LoaderCircle className="size-4 animate-spin" aria-hidden />
      {label}
    </p>
  );
}
