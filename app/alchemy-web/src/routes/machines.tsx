import { useQuery } from "@tanstack/react-query";
import { createRoute } from "@tanstack/react-router";
import { useState } from "react";

import { EngineStatusView } from "../components/EngineStatus.tsx";
import { Loading } from "../components/Loading.tsx";
import { LocalNames } from "../components/LocalNames.tsx";
import { NetworkWarnings } from "../components/NetworkWarnings.tsx";
import { PageHeader } from "../components/PageHeader.tsx";
import { PortsSection } from "../components/Ports.tsx";
import { PrivilegesSection } from "../components/Privileges.tsx";
import { ProxyWarnings } from "../components/ProxyWarnings.tsx";
import { AppUsageTable, MachineUsageView, RangePicker } from "../components/Usage.tsx";
import { type MetricsRange, api } from "../lib/api.ts";
import { useDaemonConnection } from "../lib/live.tsx";
import { liveQueries } from "../lib/queries.ts";
import { rootRoute } from "./root.tsx";

/** The machines Alchemy runs apps on. For now, only this one. */
export const machinesRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/machines",
  component: MachinesPage,
});

/** How often history is refetched: the finest tier gains a point a minute. */
const HISTORY_REFRESH_MS = 60_000;

function MachinesPage() {
  const daemon = useDaemonConnection();
  const online = daemon.kind === "online";
  const engine = useQuery({ ...liveQueries.engine, enabled: online });

  return (
    <div className="flex max-w-3xl flex-col gap-8">
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
          <p className="text-muted">Docker status appears once alchemyd is reachable.</p>
        )}
        {online && <NetworkWarnings />}
        {online && <ProxyWarnings />}
      </section>
      {online && <PrivilegesSection />}
      {online && <LocalNames />}
      {online && <Usage />}
      {online && <PortsSection />}
    </div>
  );
}

/** The machine's usage and each app's share, live, over a chosen range. */
function Usage() {
  const [range, setRange] = useState<MetricsRange>("24h");
  const metrics = useQuery(liveQueries.metrics);
  const history = useQuery({
    queryKey: ["metrics-history", range],
    queryFn: ({ signal }) => api.metricsHistory(range, undefined, signal),
    refetchInterval: HISTORY_REFRESH_MS,
  });
  const apps = useQuery(liveQueries.apps);
  const sampled = metrics.data && metrics.data.at !== null ? metrics.data : null;
  const now = sampled?.at ?? Date.now() / 1000;
  const appFor = (project: string) => {
    const matches = apps.data?.apps.filter((app) => app.project === project) ?? [];
    return matches.length === 1 ? matches[0]?.name : undefined;
  };

  return (
    <>
      <section aria-labelledby="usage" className="flex flex-col gap-3">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <h2 id="usage" className="text-base font-semibold">
            Usage
          </h2>
          <RangePicker range={range} onChange={setRange} />
        </div>
        {metrics.isError ? (
          <p className="text-muted">
            This version of alchemyd doesn't report usage. Update Alchemy to see it.
          </p>
        ) : sampled === null ? (
          <Loading label="Measuring this machine…" />
        ) : (
          <>
            {sampled.containers_in_vm && (
              <p className="text-sm text-muted">
                These are this computer's figures, not the virtual machine Docker runs your apps in.
                An app's CPU is its share of that VM.
              </p>
            )}
            <MachineUsageView metrics={sampled} history={history.data} range={range} now={now} />
          </>
        )}
      </section>
      {sampled !== null && (
        <section aria-labelledby="app-usage" className="flex flex-col gap-3">
          <h2 id="app-usage" className="text-base font-semibold">
            Apps on this machine
          </h2>
          <AppUsageTable
            metrics={sampled}
            history={history.data}
            range={range}
            now={now}
            appFor={appFor}
          />
        </section>
      )}
    </>
  );
}
