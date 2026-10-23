import { CircleCheck, CircleX, LoaderCircle } from "lucide-react";

import type { DaemonState } from "../lib/live.tsx";

/** One line saying whether `alchemyd` answers. */
export function DaemonStatus({ state }: { state: DaemonState }) {
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
