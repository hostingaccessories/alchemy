import { CircleCheck, CircleX, FlaskConical, LoaderCircle } from "lucide-react";
import { useEffect, useState } from "react";

import { api } from "./lib/api.ts";

type DaemonState =
  | { kind: "loading" }
  | { kind: "online"; version: string }
  | { kind: "offline"; error: string };

export function App() {
  const [daemon, setDaemon] = useState<DaemonState>({ kind: "loading" });

  useEffect(() => {
    const controller = new AbortController();
    api
      .version(controller.signal)
      .then(({ version }) => setDaemon({ kind: "online", version }))
      .catch((err: unknown) => {
        if (!controller.signal.aborted) {
          setDaemon({ kind: "offline", error: String(err) });
        }
      });
    return () => controller.abort();
  }, []);

  return (
    <main className="flex min-h-screen flex-col items-center justify-center gap-6 bg-stone-950 text-stone-100">
      <div className="flex items-center gap-3">
        <FlaskConical className="size-10 text-amber-500" aria-hidden />
        <h1 className="text-4xl font-semibold tracking-tight">Alchemy</h1>
      </div>
      <DaemonStatus state={daemon} />
    </main>
  );
}

function DaemonStatus({ state }: { state: DaemonState }) {
  switch (state.kind) {
    case "loading":
      return (
        <p className="flex items-center gap-2 text-stone-400">
          <LoaderCircle className="size-4 animate-spin" aria-hidden />
          Connecting to alchemyd…
        </p>
      );
    case "online":
      return (
        <p className="flex items-center gap-2 text-emerald-400">
          <CircleCheck className="size-4" aria-hidden />
          alchemyd {state.version}
        </p>
      );
    case "offline":
      return (
        <p className="flex items-center gap-2 text-rose-400" title={state.error}>
          <CircleX className="size-4" aria-hidden />
          alchemyd is not reachable
        </p>
      );
  }
}
