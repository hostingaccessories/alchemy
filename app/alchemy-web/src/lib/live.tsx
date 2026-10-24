import { useQuery, useQueryClient } from "@tanstack/react-query";
import { type ReactNode, useEffect } from "react";

import { ApiError, NO_RESPONSE, api, isSignedOut } from "./api.ts";
import { invalidateOnReconnect, sessionQuery, versionQuery, writeLiveEvent } from "./queries.ts";

/**
 * Holds the app's one `/api/events` stream and writes each event into the
 * Query cache through `liveQueries`. When the stream drops it re-probes the
 * daemon and whether the page is signed in, and when it reconnects it
 * invalidates the cache. Must sit inside `QueryClientProvider`.
 */
export function LiveProvider({ children }: { children: ReactNode }) {
  const client = useQueryClient();

  useEffect(() => {
    let dropped = false;
    return api.subscribe((event) => writeLiveEvent(client, event), {
      onOpen: () => {
        if (dropped) {
          dropped = false;
          void invalidateOnReconnect(client);
        }
      },
      onError: () => {
        dropped = true;
        void client.invalidateQueries({ queryKey: versionQuery.queryKey });
        void client.invalidateQueries({ queryKey: sessionQuery.queryKey });
      },
    });
  }, [client]);

  return children;
}

/** Whether `alchemyd` answers, as last probed. */
export type DaemonConnection =
  | { kind: "connecting" }
  | { kind: "online"; version: string }
  | { kind: "offline"; error: ApiError };

/**
 * The daemon's connection state, from the `GET /api/version` probe that
 * {@link LiveProvider} reruns whenever the event stream drops or reconnects.
 * Stays `offline` until a probe succeeds again.
 */
export function useDaemonConnection(): DaemonConnection {
  const { data, error } = useQuery(versionQuery);
  if (error) {
    return {
      kind: "offline",
      error:
        error instanceof ApiError ? error : new ApiError(NO_RESPONSE, "alchemyd is not reachable"),
    };
  }
  if (data) {
    return { kind: "online", version: data.version };
  }
  return { kind: "connecting" };
}

/**
 * Whether alchemyd refused this page as not signed in, from the
 * `GET /api/session` probe that {@link LiveProvider} reruns with the
 * version probe. `false` while the probe is pending or alchemyd is
 * unreachable, which {@link useDaemonConnection} reports instead.
 */
export function useSignedOut(): boolean {
  const { error } = useQuery(sessionQuery);
  return isSignedOut(error);
}
