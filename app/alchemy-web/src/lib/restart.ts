import { type QueryClient, queryOptions, useQuery } from "@tanstack/react-query";
import { useEffect, useState } from "react";

import type { PrivilegeList } from "./api.ts";

/**
 * How long after a privilege change asked alchemyd to restart the page keeps
 * calling it a restart. alchemyd re-executes in place within a second or two,
 * so past this it is simply unreachable and the usual banner says so.
 */
export const RESTART_GRACE_MS = 60_000;

/**
 * When this page last saw alchemyd about to restart for a privilege, in
 * milliseconds since the epoch, or `null`. It lives in the Query cache, not in
 * a module, so each `QueryClient` (and each test) has its own. Never fetched:
 * only {@link expectRestart} and {@link restartDone} write it.
 */
const restartQuery = queryOptions({
  queryKey: ["restart"],
  queryFn: () => null as number | null,
  enabled: false,
  staleTime: Infinity,
});

/** Note that alchemyd is about to restart to put a privilege in effect. */
export function expectRestart(client: QueryClient): void {
  client.setQueryData(restartQuery.queryKey, Date.now());
}

/** Forget the restart, once the event stream has reconnected signed in. */
export function restartDone(client: QueryClient): void {
  client.setQueryData(restartQuery.queryKey, null);
}

/**
 * Whether `list` says alchemyd is about to restart: an action that restarts
 * it is still pending although its prompt already succeeded.
 */
export function restartsSoon(list: PrivilegeList): boolean {
  return list.privileges.some(
    (p) => p.restarts && p.state === "pending" && p.last?.result === "done",
  );
}

/** Where a restart this page expected stands. */
export interface Restart {
  /** alchemyd should be restarting now, within {@link RESTART_GRACE_MS}. */
  restarting: boolean;
  /** It restarted (or is restarting) and the stream hasn't reconnected since. */
  restarted: boolean;
}

/** The restart this page expects, if any, re-rendering when the grace period ends. */
export function useRestart(): Restart {
  const { data } = useQuery(restartQuery);
  const at = data ?? null;
  const [, setTick] = useState(0);

  useEffect(() => {
    const left = at === null ? 0 : at + RESTART_GRACE_MS - Date.now();
    if (left <= 0) {
      return;
    }
    const timer = setTimeout(() => setTick((n) => n + 1), left);
    return () => clearTimeout(timer);
  }, [at]);

  return {
    restarting: at !== null && Date.now() < at + RESTART_GRACE_MS,
    restarted: at !== null,
  };
}
