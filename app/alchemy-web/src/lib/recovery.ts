import { type QueryClient, queryOptions, useQuery } from "@tanstack/react-query";

import type { Operation, Reconcile, ReconcileApp, ReconcileTrigger, Wake } from "./api.ts";

/**
 * The last `woke` event this page saw, or `null`. The event is a moment, sent
 * only to pages open at the time, so it lives in the Query cache like the
 * expected restart. Never fetched: only {@link noteWake} writes it.
 */
const wakeQuery = queryOptions({
  queryKey: ["last-wake"],
  queryFn: () => null as Wake | null,
  enabled: false,
  staleTime: Infinity,
});

/** Keep `wake`, so the recovery notice can say how long the computer slept. */
export function noteWake(client: QueryClient, wake: Wake): void {
  client.setQueryData(wakeQuery.queryKey, wake);
}

/** The last wake this page saw, if any. */
export function useLastWake(): Wake | null {
  return useQuery(wakeQuery).data ?? null;
}

/**
 * How far apart a wake and its reconcile may be, in seconds, to count as the
 * same: the reconcile waits for Docker to be listed again after the wake.
 */
const WAKE_MATCH_S = 300;

/** One app that didn't come back, and what to do about it. */
export interface RecoveryProblem {
  app: string;
  /** What happened and what to do, as sentences after the app's name. */
  text: string;
}

/** What a reconcile brought back and what it didn't, in words. */
export interface RecoveryReport {
  /** Something is still starting. */
  busy: boolean;
  /** Something didn't come back. */
  problem: boolean;
  /** The headline, such as "2 apps came back after this computer slept". */
  title: string;
  /** The apps that came back, by name. */
  cameBack: string[];
  /** The apps still starting, by name. */
  starting: string[];
  /** The apps left stopped because someone stopped them in Alchemy. */
  keptStopped: string[];
  problems: RecoveryProblem[];
  /** The whole report as plain text, to paste into a chat or an issue. */
  text: string;
}

/** About how long `seconds` is, in words: "45 seconds", "3 minutes", "2 hours". */
export function sleptFor(seconds: number): string {
  const unit = (n: number, word: string) => `${n} ${word}${n === 1 ? "" : "s"}`;
  if (seconds < 90) return unit(Math.max(1, Math.round(seconds)), "second");
  if (seconds < 90 * 60) return unit(Math.round(seconds / 60), "minute");
  if (seconds < 36 * 3600) return unit(Math.round(seconds / 3600), "hour");
  return unit(Math.round(seconds / 86400), "day");
}

/** What happened, as the end of a sentence: "after this computer slept for 2 hours". */
function after(trigger: ReconcileTrigger, wake: Wake | null): string {
  switch (trigger) {
    case "wake":
      return wake === null
        ? "after this computer slept"
        : `after this computer slept for ${sleptFor(wake.slept_for)}`;
    case "daemon_start":
      return "after Alchemy restarted";
    case "engine_returned":
      return "after Docker restarted";
    default:
      return "after a disruption";
  }
}

function apps(n: number): string {
  return n === 1 ? "1 app" : `${n} apps`;
}

/** `text` ending in a full stop. */
function stop(text: string): string {
  const trimmed = text.trim();
  return /[.!?…]$/.test(trimmed) ? trimmed : `${trimmed}.`;
}

function problemText(app: ReconcileApp, operation: Operation | undefined): string | null {
  switch (app.outcome) {
    case "failed": {
      const why = app.message ?? operation?.error ?? null;
      const what = why === null ? "didn't come back." : `didn't come back: ${stop(why)}`;
      return `${what} Open it to see what went wrong, then start it again.`;
    }
    case "skipped":
      if (app.reason === "partly_running") {
        return "is only partly running, so Alchemy left it alone. Open it and select Start to bring the rest back.";
      }
      if (app.reason === "status_unknown") {
        return "wasn't brought back, because Alchemy couldn't tell whether it was running. Open it to check.";
      }
      return null;
    default:
      return null;
  }
}

/**
 * What `reconcile` did, in words, or `null` when there's nothing worth
 * saying: no app was started and none needs a look, as after a short sleep
 * with every app still running. `operations` fills in why a start failed;
 * `wake` says how long the computer slept, when it is this reconcile's.
 */
export function recoveryReport(
  reconcile: Reconcile,
  operations: Operation[],
  wake: Wake | null,
): RecoveryReport | null {
  const matched =
    reconcile.trigger === "wake" &&
    wake !== null &&
    Math.abs(reconcile.at - wake.at) <= WAKE_MATCH_S
      ? wake
      : null;
  const when = after(reconcile.trigger, matched);
  const cameBack: string[] = [];
  const starting: string[] = [];
  const keptStopped: string[] = [];
  const problems: RecoveryProblem[] = [];
  for (const app of reconcile.apps) {
    if (app.outcome === "started") cameBack.push(app.app);
    else if (app.outcome === "starting") starting.push(app.app);
    else if (app.outcome === "skipped" && app.reason === "stop_intent") keptStopped.push(app.app);
    const op = operations.find((candidate) => candidate.id === app.operation);
    const text = problemText(app, op);
    if (text !== null) problems.push({ app: app.app, text });
  }
  if (cameBack.length === 0 && starting.length === 0 && problems.length === 0) {
    return null;
  }

  const title =
    starting.length > 0
      ? `Bringing ${apps(starting.length)} back ${when}…`
      : problems.length > 0
        ? `${apps(problems.length)} didn't come back ${when}`
        : `${apps(cameBack.length)} came back ${when}`;

  const lines = [`Alchemy, ${when} (${new Date(reconcile.at * 1000).toISOString()}):`];
  for (const app of cameBack) lines.push(`- ${app} came back.`);
  for (const app of starting) lines.push(`- ${app} is starting.`);
  for (const { app, text } of problems) lines.push(`- ${app} ${text}`);
  for (const app of keptStopped)
    lines.push(`- ${app} stayed stopped, because it was stopped in Alchemy.`);

  return {
    busy: starting.length > 0,
    problem: problems.length > 0,
    title,
    cameBack,
    starting,
    keptStopped,
    problems,
    text: lines.join("\n"),
  };
}

/** Which reconcile a dismissal is for, so the next one shows again. */
export function reconcileKey(reconcile: Reconcile): string {
  return `${reconcile.trigger}:${reconcile.at}`;
}
