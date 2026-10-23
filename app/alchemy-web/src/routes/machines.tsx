import { useQuery } from "@tanstack/react-query";
import { createRoute } from "@tanstack/react-router";

import { EngineStatusView } from "../components/EngineStatus.tsx";
import { PageHeader } from "../components/PageHeader.tsx";
import { useDaemonConnection } from "../lib/live.tsx";
import { liveQueries } from "../lib/queries.ts";
import { rootRoute } from "./root.tsx";

/** The machines Alchemy runs apps on. For now, only this one. */
export const machinesRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/machines",
  component: MachinesPage,
});

function MachinesPage() {
  const daemon = useDaemonConnection();
  const online = daemon.kind === "online";
  const engine = useQuery({ ...liveQueries.engine, enabled: online });

  return (
    <section aria-labelledby="this-machine" className="flex max-w-xl flex-col gap-3">
      <PageHeader title="Machines" />
      <h2 id="this-machine" className="text-base font-semibold">
        This machine
      </h2>
      {online ? (
        <div role="status" aria-label="Docker">
          <EngineStatusView status={engine.data ?? null} />
        </div>
      ) : (
        <p className="text-stone-400">Docker status appears once alchemyd is reachable.</p>
      )}
    </section>
  );
}
