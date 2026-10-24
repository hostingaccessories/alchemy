import { useQuery } from "@tanstack/react-query";

import type { ProxyListener } from "../lib/api.ts";
import { Code } from "./Code.tsx";
import { CopyButton } from "./CopyButton.tsx";
import { networkQuery } from "./NetworkWarnings.tsx";
import { Notice } from "./Notice.tsx";

/**
 * Web address listeners Alchemy can't open: port 80, which serves
 * `http://<app>.localhost`, and each app's own port, with the fix when there
 * is one. Renders nothing when they all work, or when alchemyd is too old to
 * say.
 */
export function ProxyWarnings() {
  const network = useQuery(networkQuery);
  const proxy = network.data?.proxy;
  if (proxy === undefined) {
    return null;
  }
  const listeners = [proxy.host, ...proxy.ports].filter(
    (listener) => listener.state === "unavailable",
  );
  if (listeners.length === 0) {
    return null;
  }
  return (
    <div role="group" aria-label="Web address warnings" className="flex flex-col gap-4">
      {listeners.map((listener) => (
        <Notice key={listener.port} tone="warn" title={listenerTitle(listener)}>
          {listener.message !== null && <p>{listener.message}</p>}
          {listener.fix !== null && <Fix fix={listener.fix} />}
        </Notice>
      ))}
    </div>
  );
}

function listenerTitle(listener: ProxyListener): string {
  return listener.app === null
    ? `Alchemy can't listen on port ${listener.port}`
    : `${listener.app}'s address on port ${listener.port} doesn't work`;
}

/**
 * A fix, with each `command` in it set as code. The first command can be
 * copied, so it pastes cleanly into a terminal.
 */
function Fix({ fix }: { fix: string }) {
  const parts = fix.split("`");
  const command = parts.length > 2 ? parts[1] : undefined;
  return (
    <>
      <p>{parts.map((part, i) => (i % 2 === 1 ? <Code key={i}>{part}</Code> : part))}</p>
      {command !== undefined && command !== "" && (
        <div>
          <CopyButton text={command} label="Copy command" />
        </div>
      )}
    </>
  );
}
