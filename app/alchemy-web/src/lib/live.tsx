import { type ReactNode, createContext, useContext, useEffect, useState } from "react";

import { type EngineStatus, api } from "./api.ts";

/** Whether `alchemyd` answers, as last probed. */
export type DaemonState =
  | { kind: "loading" }
  | { kind: "online"; version: string }
  | { kind: "offline"; error: string };

/** What the UI currently knows about the daemon and its Docker engine. */
export interface LiveState {
  daemon: DaemonState;
  /** `null` until the first `engine` event, or while the stream reconnects. */
  engine: EngineStatus | null;
}

const LiveContext = createContext<LiveState | null>(null);

/**
 * Holds the app-wide event stream and the daemon probe, so every route reads
 * the same state from one connection. Interim: #37 replaces this with the
 * TanStack Query data layer.
 */
export function LiveProvider({ children }: { children: ReactNode }) {
  const [daemon, setDaemon] = useState<DaemonState>({ kind: "loading" });
  const [engine, setEngine] = useState<EngineStatus | null>(null);

  useEffect(() => {
    let probe: AbortController | undefined;
    const checkDaemon = () => {
      probe?.abort();
      const controller = new AbortController();
      probe = controller;
      api
        .version(controller.signal)
        .then(({ version }) => setDaemon({ kind: "online", version }))
        .catch((err: unknown) => {
          if (!controller.signal.aborted) {
            setDaemon({ kind: "offline", error: String(err) });
          }
        });
    };

    checkDaemon();
    const unsubscribe = api.subscribe(
      (event) => {
        if (event.event === "engine") {
          setEngine(event.data);
        }
      },
      {
        onOpen: checkDaemon,
        onError: () => {
          setEngine(null);
          checkDaemon();
        },
      },
    );
    return () => {
      probe?.abort();
      unsubscribe();
    };
  }, []);

  return <LiveContext value={{ daemon, engine }}>{children}</LiveContext>;
}

/** The live daemon and engine state. Must be used under {@link LiveProvider}. */
export function useLive(): LiveState {
  const live = useContext(LiveContext);
  if (!live) {
    throw new Error("useLive must be used within LiveProvider");
  }
  return live;
}
