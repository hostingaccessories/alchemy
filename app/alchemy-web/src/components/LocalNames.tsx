import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import { type MdnsBackend, type MdnsReport, api } from "../lib/api.ts";
import { networkSettingsQuery } from "../lib/queries.ts";
import { networkQuery } from "./NetworkWarnings.tsx";
import { Notice } from "./Notice.tsx";
import { CommandFix } from "./ProxyWarnings.tsx";

/** What still works when names aren't announced, if the daemon doesn't say. */
const STILL_WORKS = "Your apps still work at their IP:port address.";

const BACKENDS: Record<MdnsBackend, string> = {
  avahi: "Announced through Avahi.",
  dns_sd: "Announced through macOS's Bonjour.",
  built_in: "Announced by Alchemy's built-in responder.",
  other: "Announced by a responder this version of Alchemy doesn't recognize.",
};

/**
 * Whether apps are announced on the LAN as `<app>.local`, and the switch for
 * it. Renders nothing when alchemyd is too old to say.
 */
export function LocalNames() {
  const network = useQuery(networkQuery);
  const mdns = network.data?.mdns;
  if (mdns === undefined) {
    return null;
  }
  return (
    <section aria-labelledby="local-names" className="flex max-w-xl flex-col gap-3">
      <h2 id="local-names" className="text-base font-semibold">
        Local names
      </h2>
      <div role="status" aria-label="Local names">
        <LocalNamesStatus report={mdns} />
      </div>
      <LocalNamesSwitch />
    </section>
  );
}

/** The daemon's mDNS report, problem first. */
function LocalNamesStatus({ report }: { report: MdnsReport }) {
  switch (report.state) {
    case "off":
      return (
        <p className="text-sm text-muted">
          Local names are off, so apps have no <span className="font-mono">.local</span> address.
          They still work on this computer and at their IP:port address.
        </p>
      );
    case "starting":
      return <Notice tone="busy" title="Alchemy is starting to announce your apps…" />;
    case "announcing":
      return (
        <Notice tone="ok" title="Devices on your network can open apps at their .local address">
          {report.backend !== null && <p>{BACKENDS[report.backend] ?? BACKENDS.other}</p>}
        </Notice>
      );
    case "unavailable":
      return (
        <Notice tone="warn" title="Alchemy can't announce your apps' .local names">
          <p>{report.message ?? STILL_WORKS}</p>
          {report.fix !== null && <CommandFix fix={report.fix} />}
        </Notice>
      );
    default:
      return (
        <Notice
          tone="warn"
          title="Local names are in a state this version of Alchemy doesn't recognize. Update Alchemy."
        />
      );
  }
}

/** The `network.mdns` setting, saved as soon as it changes. */
function LocalNamesSwitch() {
  const client = useQueryClient();
  const settings = useQuery(networkSettingsQuery);
  const save = useMutation({
    mutationFn: (mdns: boolean) => api.setNetworkSettings({ mdns }),
    onSuccess: async (next) => {
      client.setQueryData(networkSettingsQuery.queryKey, next);
      await client.invalidateQueries({ queryKey: networkQuery.queryKey });
    },
  });
  if (settings.data === undefined) {
    return null;
  }
  const checked = save.isPending ? save.variables : settings.data.mdns;
  return (
    <div className="flex flex-col gap-2">
      <label className="flex items-start gap-2 text-sm">
        <input
          type="checkbox"
          className="mt-0.5 size-4 accent-accent"
          checked={checked}
          disabled={save.isPending}
          onChange={(event) => save.mutate(event.target.checked)}
        />
        <span className="flex flex-col gap-0.5">
          <span className="font-medium">Announce apps on your network</span>
          <span className="text-muted">
            Phones and other computers on your network can then open an app at{" "}
            <span className="font-mono">http://&lt;app&gt;.local</span>.
          </span>
        </span>
      </label>
      {save.isError && (
        <div role="alert">
          <Notice tone="error" title="Couldn't change local names">
            <p>{save.error.message}</p>
          </Notice>
        </div>
      )}
    </div>
  );
}
