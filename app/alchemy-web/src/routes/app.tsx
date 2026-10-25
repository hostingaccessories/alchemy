import { useQuery } from "@tanstack/react-query";
import { Link, createRoute } from "@tanstack/react-router";
import { ExternalLink as ExternalLinkIcon } from "lucide-react";

import { Advanced } from "../components/Advanced.tsx";
import { AppFileEditor } from "../components/AppFileEditor.tsx";
import { AppLogs } from "../components/AppLogs.tsx";
import { AppActions } from "../components/AppActions.tsx";
import { AppStatus, StopReasonStatus } from "../components/AppStatus.tsx";
import { buttonClass } from "../components/Button.tsx";
import { DockerAwayNotice } from "../components/EngineStatus.tsx";
import { ErrorState } from "../components/ErrorState.tsx";
import { AddressLink } from "../components/ExternalLink.tsx";
import { Loading } from "../components/Loading.tsx";
import { Notice } from "../components/Notice.tsx";
import { PageHeader } from "../components/PageHeader.tsx";
import type { App, AppAddress, AppService, ImageSource } from "../lib/api.ts";
import { useDockerAway } from "../lib/engineStart.ts";
import { useDaemonConnection } from "../lib/live.tsx";
import { addressUrl } from "../lib/platform.ts";
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
  const dockerAway = useDockerAway(daemon.kind === "online");

  return (
    <div className="flex max-w-3xl flex-col gap-8">
      <PageHeader title={name}>
        {app && <AppStatus status={app.status} reason={app.stop_reason} dockerAway={dockerAway} />}
        {app && <OpenApp app={app} />}
      </PageHeader>
      <DockerAwayNotice />
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
          <dt className="text-muted">
            {(app.addresses?.length ?? 0) > 1 ? "Addresses" : "Address"}
          </dt>
          <dd>
            <Addresses app={app} />
          </dd>
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
    case "route":
      return "Something is wrong with this app's web address";
    default:
      return "Something is wrong with this app";
  }
}

/** Opens the app at its best address that answers; nothing while none does. */
function OpenApp({ app }: { app: App }) {
  const ready = app.addresses?.find(
    (address) => address.state === "ready" && addressUrl(address.url) !== null,
  );
  if (ready === undefined) {
    return null;
  }
  return (
    <AddressLink href={ready.url} className={buttonClass("primary")}>
      <ExternalLinkIcon className="size-4" aria-hidden />
      Open {app.name}
    </AddressLink>
  );
}

const ADDRESS_STATES: Record<AppAddress["state"], string> = {
  ready: "Ready",
  pending: "Not ready yet",
  not_running: "Not running",
  unavailable: "Doesn't work",
  other: "Unknown",
};

/**
 * Where a ready address works: `.localhost` and loopback addresses only on
 * this computer, any other, such as `<app>.local` or the LAN IP, from other
 * devices on the network too.
 */
function addressReach(url: string): string {
  const host = addressUrl(url)?.hostname ?? "";
  const local =
    host === "localhost" ||
    host.endsWith(".localhost") ||
    host.startsWith("127.") ||
    host === "[::1]";
  return local ? "On this computer only" : "From any device on your network";
}

/**
 * Each web address, best first, saying where one works or why it doesn't
 * answer.
 */
function Addresses({ app }: { app: App }) {
  const addresses = app.addresses ?? [];
  if (addresses.length === 0) {
    return (
      <span className="text-muted">
        {app.problems.some((problem) => problem.kind === "route")
          ? "None. The problem above says why."
          : "None"}
      </span>
    );
  }
  return (
    <ul className="flex flex-col gap-1">
      {addresses.map((address) => (
        <li key={address.url} className="flex flex-col">
          {addressUrl(address.url) === null ? (
            <span className="break-all">{address.url}</span>
          ) : (
            <AddressLink href={address.url} className="break-all text-accent underline">
              {address.url}
            </AddressLink>
          )}
          <span className="text-muted">
            {address.state === "ready"
              ? addressReach(address.url)
              : (address.message ?? ADDRESS_STATES[address.state] ?? ADDRESS_STATES.other)}
          </span>
        </li>
      ))}
    </ul>
  );
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
