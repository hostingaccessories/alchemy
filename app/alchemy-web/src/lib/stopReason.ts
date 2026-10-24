import type { StopReason } from "./api.ts";

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
