import { useQuery } from "@tanstack/react-query";
import { Link, createRoute } from "@tanstack/react-router";

import { Advanced } from "../components/Advanced.tsx";
import { AppFileEditor } from "../components/AppFileEditor.tsx";
import { AppLogs } from "../components/AppLogs.tsx";
import { AppActions } from "../components/AppActions.tsx";
import { AppStatus, StopReasonStatus } from "../components/AppStatus.tsx";
import { ErrorState } from "../components/ErrorState.tsx";
import { Loading } from "../components/Loading.tsx";
import { Notice } from "../components/Notice.tsx";
import { PageHeader } from "../components/PageHeader.tsx";
import type { App, AppService, ImageSource } from "../lib/api.ts";
import { useDaemonConnection } from "../lib/live.tsx";
import { liveQueries } from "../lib/queries.ts";
import { serviceStopReason } from "../lib/stopReason.ts";
import { formatBytes, formatPercent, formatRate, sumMeasured } from "../lib/units.ts";
import { rootRoute } from "./root.tsx";

/**
 * One app: how it's doing, its services, and where it lives. Read from the
 * live app list, so it updates as the app does, with its lifecycle actions
 * and their progress, and its logs. The compose file editor goes on this
 * page too.
 */
export const appRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/apps/$name",
  component: AppPage,
});

const IMAGES: Record<ImageSource, string> = {
  registry: "Downloaded",
  build: "Built from the app's files",
};

function AppPage() {
  const { name } = appRoute.useParams();
  const daemon = useDaemonConnection();
  const apps = useQuery({ ...liveQueries.apps, enabled: daemon.kind === "online" });
  const app = apps.data?.apps.find((candidate) => candidate.name === name);

  return (
    <div className="flex max-w-3xl flex-col gap-8">
      <PageHeader title={name}>
        {app && <AppStatus status={app.status} reason={app.stop_reason} />}
      </PageHeader>
      {daemon.kind !== "online" && apps.data === undefined ? (
        <p className="text-muted">This app appears once alchemyd is reachable.</p>
      ) : apps.isError ? (
        <ErrorState
          error={apps.error}
          title="Couldn't load this app"
          onRetry={() => void apps.refetch()}
        />
      ) : apps.data === undefined ? (
        <Loading label="Loading this app…" />
      ) : app === undefined ? (
        <div className="flex flex-col gap-2">
          <p className="text-muted">There's no app named “{name}”.</p>
          <p>
            <Link to="/apps" className="text-accent underline">
              Go to Apps
            </Link>
          </p>
        </div>
      ) : (
        <AppView app={app} />
      )}
    </div>
  );
}

function AppView({ app }: { app: App }) {
  return (
    <>
      {app.problems.length > 0 && (
        <section aria-label="Problems" className="flex flex-col gap-3">
          {app.problems.map((problem) => (
            <Notice
              key={problem.kind + problem.message}
              tone={problem.kind === "compose_error" ? "error" : "warn"}
              title={problemTitle(problem.kind)}
            >
              <p>{problem.message}</p>
            </Notice>
          ))}
        </section>
      )}
      <AppActions app={app} />
      <section aria-labelledby="services" className="flex flex-col gap-3">
        <h2 id="services" className="text-base font-semibold">
          Services
        </h2>
        {app.services.length === 0 ? (
          <p className="text-muted">
            {app.project === null
              ? "Alchemy can't list this app's services until Compose can read it."
              : "This app has no services."}
          </p>
        ) : (
          <ul className="flex flex-col gap-2">
            {app.services.map((service) => (
              <ServiceRow key={service.name} service={service} />
            ))}
          </ul>
        )}
      </section>
      {app.project !== null && <AppUsage project={app.project} />}
      <AppLogs app={app} />
      <AppFileEditor app={app} />
      <section aria-labelledby="details" className="flex flex-col gap-3">
        <h2 id="details" className="text-base font-semibold">
          Where it lives
        </h2>
        <dl className="grid grid-cols-[max-content_1fr] gap-x-4 gap-y-1 text-sm">
          <dt className="text-muted">Folder</dt>
          <dd className="break-all">{app.dir}</dd>
          <dt className="text-muted">Compose file</dt>
          <dd>{app.compose_file}</dd>
          <dt className="text-muted">Images</dt>
          <dd>{app.images ? IMAGES[app.images] : "Not known yet"}</dd>
        </dl>
        <Advanced>
          <dl className="grid grid-cols-[max-content_1fr] gap-x-4 gap-y-1 text-sm">
            <dt className="text-muted">Compose project</dt>
            <dd>{app.project ?? "Not known yet"}</dd>
            {app.services.flatMap((service) =>
              service.containers.map((container) => [
                <dt key={`${container.id}-t`} className="text-muted">
                  {service.name}
                </dt>,
                <dd key={`${container.id}-d`} className="break-all">
                  {container.name} ({container.state}
                  {container.image ? `, ${container.image}` : ""})
                </dd>,
              ]),
            )}
          </dl>
        </Advanced>
      </section>
    </>
  );
}

function problemTitle(kind: App["problems"][number]["kind"]): string {
  switch (kind) {
    case "compose_error":
      return "Compose can't read this app";
    case "compose_unavailable":
      return "Alchemy couldn't check this app";
    case "shared_project":
      return "Another app uses the same name in Compose";
    case "network_overlap":
      return "One of this app's networks overlaps your network";
    case "port_conflict":
      return "A port this app publishes is already taken";
    default:
      return "Something is wrong with this app";
  }
}

function ServiceRow({ service }: { service: AppService }) {
  const health = service.containers.find((c) => c.health !== null)?.health;
  return (
    <li className="flex items-center gap-4 rounded-lg border border-border bg-surface-raised px-4 py-2">
      <span className="min-w-0 flex-1 font-medium">{service.name}</span>
      {health === "unhealthy" && <span className="text-sm text-error">Unhealthy</span>}
      <StopReasonStatus reason={serviceStopReason(service)} />
    </li>
  );
}

/** The app's latest CPU, memory, and network use, when it's running. */
function AppUsage({ project }: { project: string }) {
  const metrics = useQuery(liveQueries.metrics);
  const usage = metrics.data?.projects.find((p) => p.name === project);
  if (usage === undefined) {
    return null;
  }
  return (
    <section aria-labelledby="usage" className="flex flex-col gap-3">
      <h2 id="usage" className="text-base font-semibold">
        Usage
      </h2>
      <dl className="grid grid-cols-[max-content_1fr] gap-x-4 gap-y-1 text-sm">
        <dt className="text-muted">CPU</dt>
        <dd className="tabular-nums">{formatPercent(usage.cpu)}</dd>
        <dt className="text-muted">Memory</dt>
        <dd className="tabular-nums">{formatBytes(usage.memory)}</dd>
        <dt className="text-muted">Network</dt>
        <dd className="tabular-nums">{formatRate(sumMeasured(usage.net_rx, usage.net_tx))}</dd>
      </dl>
    </section>
  );
}
