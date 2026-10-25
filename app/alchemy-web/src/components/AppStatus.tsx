import {
  CircleAlert,
  CircleCheck,
  CircleDot,
  CircleQuestionMark,
  Hourglass,
  type LucideIcon,
} from "lucide-react";

import type { StackStatus, StopReason } from "../lib/api.ts";
import { appStatusLabel, describeStopReason, isStopProblem } from "../lib/stopReason.ts";

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

const PROBLEM = { icon: CircleAlert, className: "text-error" };

/**
 * An app's status as an icon and words, saying why it isn't running when
 * `reason` is a crash or a stop made outside Alchemy. With `dockerAway`, an
 * app whose status alchemyd can't know reads as waiting for Docker.
 */
export function AppStatus({
  status,
  reason,
  dockerAway = false,
}: {
  status: StackStatus;
  reason?: StopReason;
  dockerAway?: boolean;
}) {
  if (dockerAway && (STATUSES[status] === undefined || status === "unknown")) {
    return (
      <span className="inline-flex shrink-0 items-center gap-1.5 text-sm text-muted">
        <Hourglass className="size-4" aria-hidden />
        Waiting for Docker
      </span>
    );
  }
  const known = STATUSES[status] ?? STATUSES.unknown;
  const problem = status !== "running" && status !== "unknown" && isStopProblem(reason);
  const { icon: Icon, className } = problem ? PROBLEM : known;
  return (
    <span className={`inline-flex shrink-0 items-center gap-1.5 text-sm ${className}`}>
      <Icon className="size-4" aria-hidden />
      {appStatusLabel(status, reason)}
    </span>
  );
}

/** Why something isn't running, such as "Crashed (out of memory)", as an icon and words. */
export function StopReasonStatus({ reason }: { reason: StopReason }) {
  const { icon: Icon, className } =
    reason.kind === "running"
      ? STATUSES.running
      : isStopProblem(reason)
        ? PROBLEM
        : STATUSES.stopped;
  return (
    <span className={`inline-flex shrink-0 items-center gap-1.5 text-sm ${className}`}>
      <Icon className="size-4" aria-hidden />
      {describeStopReason(reason)}
    </span>
  );
}
