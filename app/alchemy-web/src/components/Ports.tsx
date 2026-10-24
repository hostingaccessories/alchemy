import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";

import {
  type PortClaim,
  type PortHolder,
  type PortSource,
  type PublishedPort,
  api,
} from "../lib/api.ts";
import { liveQueries } from "../lib/queries.ts";
import { Loading } from "./Loading.tsx";
import { Notice } from "./Notice.tsx";

/** How often the ports are refetched: listeners come and go unannounced. */
export const PORTS_REFRESH_MS = 10_000;

/** `8080/tcp`, or `8000-8010/udp` for a range. */
export function portLabel(port: PublishedPort): string {
  return `${port.published ?? "any"}/${port.protocol}`;
}

/** The address a port is bound on, or "All" for every address. */
export function addressLabel(port: PublishedPort): string {
  return port.host_ip ?? "All";
}

const SOURCES: Record<PortSource, string> = {
  live: "In use",
  reserved: "Reserved",
  declared: "Declared",
};

/** What a port's source means to someone reading the table. */
export function sourceLabel(source: PortSource): string {
  return SOURCES[source] ?? "Unknown";
}

/** Who holds a port, as a phrase. Apps are linked separately. */
export function holderLabel(holder: PortHolder): string {
  switch (holder.kind) {
    case "app":
      return `${holder.service} of ${holder.app}`;
    case "container":
      return `Container ${holder.name}`;
    case "reservation":
      return `Reserved for ${holder.owner}`;
    case "program": {
      const pid = holder.pid === null ? null : `pid ${holder.pid}`;
      if (holder.process !== null) {
        return pid === null ? holder.process : `${holder.process} (${pid})`;
      }
      return pid === null ? "Another program" : `Another program (${pid})`;
    }
    default:
      return "Something this version of Alchemy doesn't recognize";
  }
}

/** Who holds a port, linking an app's service to the app's page. */
function Holder({ holder, apps }: { holder: PortHolder; apps: ReadonlySet<string> }) {
  if (holder.kind === "app" && apps.has(holder.app)) {
    return (
      <>
        {holder.service} of{" "}
        <Link
          to="/apps/$name"
          params={{ name: holder.app }}
          className="text-accent hover:underline"
        >
          {holder.app}
        </Link>
      </>
    );
  }
  return <>{holderLabel(holder)}</>;
}

/** Every claimed host port and who holds it, one row each. */
export function PortsTable({
  ports,
  apps = new Set(),
}: {
  ports: PortClaim[];
  /** Names of the apps with a page to link to. */
  apps?: ReadonlySet<string>;
}) {
  if (ports.length === 0) {
    return <p className="text-muted">No ports are in use, reserved, or declared.</p>;
  }
  return (
    <table className="w-full text-sm">
      <thead className="text-left text-muted">
        <tr>
          <th className="py-1 font-normal">Port</th>
          <th className="py-1 font-normal">Address</th>
          <th className="py-1 font-normal">Status</th>
          <th className="py-1 font-normal">Held by</th>
        </tr>
      </thead>
      <tbody>
        {ports.map((claim) => {
          const port = portLabel(claim.port);
          const address = addressLabel(claim.port);
          return (
            <tr
              key={`${port} ${address} ${claim.source} ${holderLabel(claim.holder)}`}
              className="border-t border-border"
            >
              <td className="py-1.5 pr-3 font-medium tabular-nums">{port}</td>
              <td className="py-1.5 pr-3 tabular-nums">{address}</td>
              <td className="py-1.5 pr-3">{sourceLabel(claim.source)}</td>
              <td className="py-1.5">
                <Holder holder={claim.holder} apps={apps} />
              </td>
            </tr>
          );
        })}
      </tbody>
    </table>
  );
}

/** The machine page's Ports panel, refreshed every few seconds. */
export function PortsSection() {
  const ports = useQuery({
    queryKey: ["ports"],
    queryFn: ({ signal }) => api.ports(signal),
    refetchInterval: PORTS_REFRESH_MS,
  });
  const apps = useQuery(liveQueries.apps);
  const names = new Set(apps.data?.apps.map((app) => app.name) ?? []);

  return (
    <section aria-labelledby="ports" className="flex flex-col gap-3">
      <h2 id="ports" className="text-base font-semibold">
        Ports
      </h2>
      {ports.isError ? (
        <p className="text-muted">
          This version of alchemyd doesn't list ports. Update Alchemy to see them.
        </p>
      ) : ports.data === undefined ? (
        <Loading label="Checking ports…" />
      ) : (
        <>
          {ports.data.warnings.map((warning) => (
            <div key={warning} role="status">
              <Notice tone="warn" title={warning} />
            </div>
          ))}
          <PortsTable ports={ports.data.ports} apps={names} />
          {ports.data.ports.some((claim) => claim.source === "declared") && (
            <p className="text-sm text-muted">
              Declared ports belong to apps that aren't running. Alchemy won't hand them to anything
              else.
            </p>
          )}
        </>
      )}
    </section>
  );
}
