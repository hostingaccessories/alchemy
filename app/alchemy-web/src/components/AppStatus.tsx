import {
  CircleAlert,
  CircleCheck,
  CircleDot,
  CircleQuestionMark,
  type LucideIcon,
} from "lucide-react";

import type { StackStatus } from "../lib/api.ts";

const STATUSES: Record<StackStatus, { label: string; icon: LucideIcon; className: string }> = {
  running: { label: "Running", icon: CircleCheck, className: "text-ok" },
  partial: { label: "Partly running", icon: CircleAlert, className: "text-warn" },
  stopped: { label: "Stopped", icon: CircleDot, className: "text-muted" },
  unknown: { label: "Status unknown", icon: CircleQuestionMark, className: "text-muted" },
};

/** The words for an app's status. A status this build doesn't know reads as unknown. */
export function statusLabel(status: StackStatus): string {
  return (STATUSES[status] ?? STATUSES.unknown).label;
}

/** An app's status as an icon and a word. */
export function AppStatus({ status }: { status: StackStatus }) {
  const { label, icon: Icon, className } = STATUSES[status] ?? STATUSES.unknown;
  return (
    <span className={`inline-flex shrink-0 items-center gap-1.5 text-sm ${className}`}>
      <Icon className="size-4" aria-hidden />
      {label}
    </span>
  );
}
