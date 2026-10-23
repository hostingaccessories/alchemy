import { createRoute } from "@tanstack/react-router";

import { EngineStatusView } from "../components/EngineStatus.tsx";
import { PageHeader } from "../components/PageHeader.tsx";
import { useLive } from "../lib/live.tsx";
import { rootRoute } from "./root.tsx";

/** The machines Alchemy runs apps on. For now, only this one. */
export const machinesRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/machines",
  component: MachinesPage,
});

function MachinesPage() {
  const { daemon, engine } = useLive();

  return (
    <section aria-labelledby="this-machine" className="flex max-w-xl flex-col gap-3">
      <PageHeader title="Machines" />
      <h2 id="this-machine" className="text-base font-semibold">
        This machine
      </h2>
      {daemon.kind === "online" ? (
        <div role="status" aria-label="Docker">
          <EngineStatusView status={engine} />
        </div>
      ) : (
        <p className="text-stone-400">Docker status appears once alchemyd is reachable.</p>
      )}
    </section>
  );
}
