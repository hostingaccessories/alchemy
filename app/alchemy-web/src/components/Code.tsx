import type { ReactNode } from "react";

/** A command, path, or other literal inside running text. */
export function Code({ children }: { children: ReactNode }) {
  return <code className="rounded bg-surface-muted px-1 py-0.5 text-text">{children}</code>;
}
