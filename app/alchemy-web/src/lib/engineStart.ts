import {
  type MutationState,
  type QueryClient,
  queryOptions,
  useMutation,
  useMutationState,
  useQuery,
  useQueryClient,
} from "@tanstack/react-query";
import { useEffect, useState } from "react";

import { type EngineStart, type EngineState, type EngineStatus, api } from "./api.ts";
import { liveQueries } from "./queries.ts";

/**
 * How long after asking for Docker Desktop the page keeps showing its
 * progress. A cold start takes up to a minute or so; past this, something
 * in Docker Desktop probably needs the person, so the page says so.
 */
export const DOCKER_START_TIMEOUT_MS = 120_000;

/**
 * When this page last asked alchemyd to start Docker Desktop and it said
 * `starting`, in milliseconds since the epoch, or `null`. It lives in the
 * Query cache, like the expected restart, so every notice on every page
 * agrees. Never fetched: only {@link noteDockerStarting} and
 * {@link dockerStartDone} write it.
 */
const startingQuery = queryOptions({
  queryKey: ["engine-start"],
  queryFn: () => null as number | null,
  enabled: false,
  staleTime: Infinity,
});

/** Note that Docker Desktop is on its way up. */
export function noteDockerStarting(client: QueryClient): void {
  client.setQueryData(startingQuery.queryKey, Date.now());
}

/** Forget the start, once Docker is ready. */
export function dockerStartDone(client: QueryClient): void {
  client.setQueryData(startingQuery.queryKey, null);
}

/** The engine states in which Docker can't run apps. Unknown states are not among them. */
const AWAY: ReadonlySet<EngineState> = new Set([
  "not_installed",
  "not_running",
  "starting",
  "unsupported",
]);

/**
 * Whether `status` says Docker can't run apps right now. `false` while
 * there's no report yet, or for a state this build doesn't know.
 */
export function isDockerAway(status: EngineStatus | null | undefined): boolean {
  return status != null && AWAY.has(status.state);
}

/** Whether Docker can't run apps right now, from the live `engine` query. */
export function useDockerAway(enabled = true): boolean {
  const engine = useQuery({ ...liveQueries.engine, enabled });
  return isDockerAway(engine.data);
}

/** A request to start Docker, and how it went. */
type StartRequest = MutationState<EngineStart, Error, void>;

const MUTATION_KEY = ["engine-start"];

/** Where a start of Docker Desktop this page asked for stands. */
export interface DockerStart {
  /** Ask alchemyd to start Docker Desktop. */
  start: () => void;
  /** The request is on its way. */
  sending: boolean;
  /** Docker Desktop is starting, within {@link DOCKER_START_TIMEOUT_MS}. */
  starting: boolean;
  /** It was asked to start, and isn't ready after {@link DOCKER_START_TIMEOUT_MS}. */
  timedOut: boolean;
  /** The last answer, when it wasn't `starting`, such as `unsupported` with a command. */
  answer: EngineStart | null;
  /** Why the last request failed, such as Docker Desktop not being installed. */
  error: Error | null;
}

/**
 * Start Docker Desktop through `POST /api/engine/start`, and follow it until
 * the `engine` event says Docker is ready or the timeout passes. Every
 * component that calls it shares the same request and progress.
 */
export function useDockerStart(): DockerStart {
  const client = useQueryClient();
  const mutation = useMutation({
    mutationKey: MUTATION_KEY,
    mutationFn: () => api.startEngine(),
    onSuccess: (answer) => {
      if (answer.result === "starting") {
        noteDockerStarting(client);
      }
    },
  });
  const requests = useMutationState({
    filters: { mutationKey: MUTATION_KEY },
    select: (m) => m.state as StartRequest,
  });
  const latest = requests.at(-1);
  const { data } = useQuery(startingQuery);
  const since = data ?? null;
  const [, setTick] = useState(0);

  useEffect(() => {
    const left = since === null ? 0 : since + DOCKER_START_TIMEOUT_MS - Date.now();
    if (left <= 0) {
      return;
    }
    const timer = setTimeout(() => setTick((n) => n + 1), left);
    return () => clearTimeout(timer);
  }, [since]);

  const within = since !== null && Date.now() < since + DOCKER_START_TIMEOUT_MS;
  const answer = latest?.status === "success" ? (latest.data ?? null) : null;
  return {
    start: () => mutation.mutate(),
    sending: latest?.status === "pending",
    starting: within,
    timedOut: since !== null && !within,
    answer: answer?.result === "starting" ? null : answer,
    error: latest?.status === "error" ? latest.error : null,
  };
}
