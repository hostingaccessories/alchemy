import type { AppService, StackStatus, StopReason } from "./api.ts";

/**
 * A stop reason in plain words, problem first, such as "Crashed (out of
 * memory)" or "Keeps crashing". Matches `StopReason`'s `Display` in
 * `alchemy-core`, which the CLI prints. A missing reason, from a daemon older
 * than stop reasons, reads as "Stopped".
 */
export function describeStopReason(reason: StopReason | undefined): string {
  switch (reason?.kind) {
    case "running":
      return "Running";
    case "paused":
      return "Paused";
    case "not_started":
      return "Not running";
    case "stopped_outside":
      return "Stopped outside Alchemy";
    case "finished":
      return "Finished";
    case "crashed":
      return `Crashed${detail(reason)}`;
    case "crash_looping":
      return `Keeps crashing${detail(reason)}`;
    case "stopped_by_alchemy":
    case "unknown":
    case undefined:
      return "Stopped";
  }
}

/** Whether the reason is a problem someone should look at. */
export function isStopProblem(reason: StopReason | undefined): boolean {
  return reason?.kind === "crashed" || reason?.kind === "crash_looping";
}

function detail(reason: { exit_code: number | null; oom: boolean }): string {
  if (reason.oom) return " (out of memory)";
  if (reason.exit_code !== null) return ` (exit code ${reason.exit_code})`;
  return "";
}

const STATUS_WORDS: Record<StackStatus, string> = {
  running: "Running",
  partial: "Partly running",
  stopped: "Stopped",
  unknown: "Status unknown",
};

/**
 * An app's status in words, with why it isn't running when that is a
 * problem or was done outside Alchemy, such as "Stopped, keeps crashing".
 * Plain stops and finished jobs need no explaining. Matches the CLI's
 * `app_status`.
 */
export function appStatusLabel(status: StackStatus, reason: StopReason | undefined): string {
  const word = STATUS_WORDS[status] ?? STATUS_WORDS.unknown;
  if (status !== "partial" && status !== "stopped") return word;
  if (status === "stopped" && reason?.kind === "stopped_outside") return "Stopped outside Alchemy";
  if (isStopProblem(reason) || reason?.kind === "stopped_outside") {
    const why = describeStopReason(reason);
    return `${word}, ${why.charAt(0).toLowerCase()}${why.slice(1)}`;
  }
  return word;
}

/**
 * The reason worth showing for a Compose project Alchemy doesn't manage.
 * Every stop of one is made outside Alchemy, so only a crash says more than
 * its status. Matches the CLI's `unmanaged_status`.
 */
export function unmanagedStopReason(reason: StopReason | undefined): StopReason | undefined {
  return reason?.kind === "stopped_outside" ? undefined : reason;
}

/**
 * Why a service isn't running: a crash in any of its containers first,
 * else its first container's reason, else not started.
 */
export function serviceStopReason(service: AppService): StopReason {
  if (service.containers.some((c) => c.state === "running")) return { kind: "running" };
  const reasons = service.containers.map((c) => c.stop_reason ?? { kind: "unknown" as const });
  return reasons.find(isStopProblem) ?? reasons[0] ?? { kind: "not_started" };
}
