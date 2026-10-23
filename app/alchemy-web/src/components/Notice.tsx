import { CircleAlert, CircleCheck, CircleX, LoaderCircle, type LucideIcon } from "lucide-react";
import type { ReactNode } from "react";

import { Advanced } from "./Advanced.tsx";

/** What a {@link Notice} reports: fine, worth a look, broken, or in progress. */
export type Tone = "ok" | "warn" | "error" | "busy";

const TONES: Record<Tone, { icon: LucideIcon; className: string }> = {
  ok: { icon: CircleCheck, className: "text-ok" },
  warn: { icon: CircleAlert, className: "text-warn" },
  error: { icon: CircleX, className: "text-error" },
  busy: { icon: LoaderCircle, className: "text-muted" },
};

/**
 * Problem-first status: one line saying what is wrong (or right), the next
 * step underneath, and anything more technical folded under "Details".
 * Callers that update live wrap it in a `role="status"` or `role="alert"`
 * region.
 */
export function Notice({
  tone,
  title,
  children,
  action,
  detail,
}: {
  tone: Tone;
  /** The one thing to know, as a sentence. */
  title: string;
  /** The next step, or a short explanation. */
  children?: ReactNode;
  /** A button or link that takes the next step. */
  action?: ReactNode;
  /** Technical detail, shown on request. */
  detail?: ReactNode;
}) {
  const { icon: Icon, className } = TONES[tone];
  return (
    <div className="flex flex-col gap-2">
      <p className={`flex items-center gap-2 ${className}`}>
        {tone === "busy" ? (
          <Icon className="size-4 shrink-0 animate-spin" role="img" aria-label="In progress" />
        ) : (
          <Icon className="size-4 shrink-0" aria-hidden />
        )}
        {title}
      </p>
      {children && <div className="flex flex-col gap-1 text-sm text-muted">{children}</div>}
      {action && <div className="flex flex-wrap items-center gap-2">{action}</div>}
      {detail && <Advanced label="Details">{detail}</Advanced>}
    </div>
  );
}
