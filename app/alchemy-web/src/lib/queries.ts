import {
  type DataTag,
  type DefaultError,
  type QueryClient,
  type QueryKey,
  hashKey,
  queryOptions,
} from "@tanstack/react-query";

import {
  type ApiEvent,
  type ApiEventData,
  type ApiEventName,
  type Operation,
  type OperationList,
  api,
} from "./api.ts";

/**
 * `GET /api/version`, which doubles as the daemon reachability probe behind
 * `useDaemonConnection`. No retries, so an unreachable daemon shows at once.
 */
export const versionQuery = queryOptions({
  queryKey: ["version"],
  queryFn: ({ signal }) => api.version(signal),
  retry: false,
});

/**
 * `GET /api/session`: whether this page is signed in to alchemyd. It fails
 * with a 401 `unauthorized` (`isSignedOut`) until the browser has been
 * through `alchemy open`, or while the shell lacks the daemon's token. Rerun
 * with the version probe whenever the event stream drops.
 */
export const sessionQuery = queryOptions({
  queryKey: ["session"],
  queryFn: ({ signal }) => api.session(signal),
  retry: false,
});

/**
 * `GET /api/settings/apps`: the app folders in effect. Saving them replaces
 * this query's data with the daemon's answer.
 */
export const appSettingsQuery = queryOptions({
  queryKey: ["settings", "apps"],
  queryFn: ({ signal }) => api.appSettings(signal),
});

/** A query whose data is one event's payload. */
export interface LiveQuery<K extends ApiEventName> {
  queryKey: DataTag<QueryKey, ApiEventData<K>, DefaultError>;
}

/**
 * The query each event on `/api/events` keeps current: every event's payload
 * replaces its query's data. Pages read one with
 * `useQuery(liveQueries.engine)`.
 *
 * Each `queryFn` fetches the same value over REST, for a page that mounts
 * before the first event or after the cache dropped it. `staleTime` is
 * `Infinity` because the stream, not a refetch, keeps the data fresh.
 */
export const liveQueries = {
  engine: queryOptions({
    queryKey: ["engine"],
    queryFn: ({ signal }) => api.engine(signal),
    staleTime: Infinity,
  }),
  metrics: queryOptions({
    queryKey: ["metrics"],
    queryFn: ({ signal }) => api.metrics(signal),
    staleTime: Infinity,
  }),
  apps: queryOptions({
    queryKey: ["apps"],
    queryFn: ({ signal }) => api.apps(signal),
    staleTime: Infinity,
  }),
  operations: queryOptions({
    queryKey: ["operations"],
    queryFn: ({ signal }) => api.operations(signal),
    staleTime: Infinity,
  }),
} satisfies { [K in ApiEventName]: LiveQuery<K> };

/**
 * Store one event's payload as its {@link liveQueries} entry's data. The
 * `satisfies` above ties each entry's data type to its event's payload;
 * TypeScript can't carry that pairing through `event.event` here.
 */
export function writeLiveEvent(client: QueryClient, event: ApiEvent): void {
  const { queryKey }: { queryKey: QueryKey } = liveQueries[event.event];
  client.setQueryData<ApiEvent["data"]>(queryKey, event.data);
}

/**
 * Put `operation`, just started, first in the operations query, so its
 * progress shows before the `operations` event brings it.
 */
export function showOperation(client: QueryClient, operation: Operation): void {
  client.setQueryData<OperationList>(liveQueries.operations.queryKey, (list) => ({
    operations: [operation, ...(list?.operations ?? []).filter((op) => op.id !== operation.id)],
  }));
}

const LIVE_KEYS = new Set(Object.values(liveQueries).map(({ queryKey }) => hashKey(queryKey)));

function isLive(queryKey: QueryKey): boolean {
  return LIVE_KEYS.has(hashKey(queryKey));
}

/**
 * Mark every query stale after the stream reconnects, since the daemon may
 * have restarted. Queries the stream does not feed are refetched now; live
 * queries are not, because the daemon resends every event's current value on
 * connect and a refetch would race it.
 */
export async function invalidateOnReconnect(client: QueryClient): Promise<void> {
  await Promise.all([
    client.invalidateQueries({ predicate: ({ queryKey }) => !isLive(queryKey) }),
    client.invalidateQueries({
      predicate: ({ queryKey }) => isLive(queryKey),
      refetchType: "none",
    }),
  ]);
}
