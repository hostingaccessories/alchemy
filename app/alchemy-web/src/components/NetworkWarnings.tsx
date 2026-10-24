import { useQuery } from "@tanstack/react-query";

import { type NetworkWarning, type NetworkWarningKind, api } from "../lib/api.ts";
import { CopyButton } from "./CopyButton.tsx";
import { Notice } from "./Notice.tsx";

/** How often the report is refetched: the daemon checks every 30 seconds. */
export const NETWORK_REFRESH_MS = 30_000;

/**
 * What overlaps between Docker's networks and this machine's, each with its
 * fix. Renders nothing when nothing does, or when alchemyd is too old to say.
 */
export function NetworkWarnings() {
  const network = useQuery({
    queryKey: ["network"],
    queryFn: ({ signal }) => api.network(signal),
    refetchInterval: NETWORK_REFRESH_MS,
  });
  const warnings = network.data?.warnings ?? [];
  if (warnings.length === 0) {
    return null;
  }
  const shown = new Set<string>();
  return (
    <div role="group" aria-label="Network warnings" className="flex flex-col gap-4">
      {warnings.map((warning) => {
        const repeated = warning.fix !== null && shown.has(warning.fix);
        if (warning.fix !== null) {
          shown.add(warning.fix);
        }
        return (
          <Notice
            key={warning.kind + warning.message}
            tone="warn"
            title={warningTitle(warning.kind)}
          >
            <p>{warning.message}</p>
            {repeated ? <p>The same change as above fixes this.</p> : <Fix warning={warning} />}
          </Notice>
        );
      })}
    </div>
  );
}

/** A fix, with any `daemon.json` fragment in it set apart and copyable. */
function Fix({ warning }: { warning: NetworkWarning }) {
  if (warning.fix === null) {
    return null;
  }
  const lines = warning.fix.split("\n");
  const start = lines.indexOf("{");
  const end = lines.lastIndexOf("}");
  if (start === -1 || end < start) {
    return <p>{warning.fix}</p>;
  }
  const json = lines.slice(start, end + 1).join("\n");
  const before = lines.slice(0, start).join(" ");
  const after = lines.slice(end + 1).join(" ");
  return (
    <>
      {before !== "" && <p>{before}</p>}
      <pre className="overflow-x-auto rounded bg-surface-muted p-2 text-xs text-text">{json}</pre>
      <div>
        <CopyButton text={json} label="Copy settings" />
      </div>
      {after !== "" && <p>{after}</p>}
    </>
  );
}

function warningTitle(kind: NetworkWarningKind): string {
  switch (kind) {
    case "bridge_overlap":
      return "Docker's default network overlaps your network";
    case "pool_overlap":
      return "Docker may put new networks on top of yours";
    case "network_overlap":
      return "A Docker network overlaps your network";
    case "alchemy_network_overlap":
      return "Alchemy's network overlaps your network";
    case "no_free_subnet":
    case "create_failed":
      return "Alchemy couldn't create its network";
    case "host_unreadable":
      return "Alchemy couldn't read this machine's networks";
    case "engine_unreadable":
      return "Alchemy couldn't read Docker's networks";
    default:
      return "Something is wrong with the networks";
  }
}
