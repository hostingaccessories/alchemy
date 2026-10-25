import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { X } from "lucide-react";
import { useState } from "react";

import { useDaemonConnection } from "../lib/live.tsx";
import { liveQueries } from "../lib/queries.ts";
import { reconcileKey, recoveryReport, useLastWake } from "../lib/recovery.ts";
import { Button } from "./Button.tsx";
import { CopyButton } from "./CopyButton.tsx";
import { Notice } from "./Notice.tsx";

/** The `localStorage` key holding the last recovery notice dismissed. */
export const DISMISSED_KEY = "alchemy.recovery.dismissed";

function readDismissed(): string | null {
  try {
    return window.localStorage.getItem(DISMISSED_KEY);
  } catch {
    return null;
  }
}

/** Remember a dismissal; without storage it lasts until the page reloads. */
function writeDismissed(key: string): void {
  try {
    window.localStorage.setItem(DISMISSED_KEY, key);
  } catch {
    return;
  }
}

/**
 * What came back after alchemyd restarted, the computer woke, or Docker
 * returned, and for each app that didn't, why and what to do. It follows
 * the live `reconcile` event, can be copied whole for an AI assistant or an
 * issue, and stays dismissed until the next reconcile.
 */
export function RecoveryNotice() {
  const daemon = useDaemonConnection();
  const online = daemon.kind === "online";
  const reconcile = useQuery({ ...liveQueries.reconcile, enabled: online });
  const operations = useQuery({ ...liveQueries.operations, enabled: online });
  const wake = useLastWake();
  const [dismissed, setDismissed] = useState(readDismissed);
  const last = reconcile.data?.last ?? null;
  if (!online || last === null || !Array.isArray(last.apps)) {
    return null;
  }
  const key = reconcileKey(last);
  const report = recoveryReport(last, operations.data?.operations ?? [], wake);
  if (report === null || dismissed === key) {
    return null;
  }
  const dismiss = () => {
    writeDismissed(key);
    setDismissed(key);
  };

  return (
    <section
      aria-label="What came back"
      className="flex flex-col gap-3 rounded-lg border border-border bg-surface-raised p-4"
    >
      <div className="flex items-start gap-2">
        <div role="status" className="min-w-0 flex-1">
          <Notice tone={report.busy ? "busy" : report.problem ? "warn" : "ok"} title={report.title}>
            {report.problems.length > 0 && (
              <ul className="flex flex-col gap-1">
                {report.problems.map(({ app, text }) => (
                  <li key={app}>
                    <AppLink name={app} /> {text}
                  </li>
                ))}
              </ul>
            )}
            {report.cameBack.length > 0 && <p>Came back: {names(report.cameBack)}.</p>}
            {report.starting.length > 0 && <p>Starting: {names(report.starting)}…</p>}
            {report.keptStopped.length > 0 && (
              <p>
                Stayed stopped, because they were stopped in Alchemy: {names(report.keptStopped)}.
              </p>
            )}
          </Notice>
        </div>
        <Button aria-label="Dismiss" title="Dismiss" className="px-1.5" onClick={dismiss}>
          <X className="size-4" aria-hidden />
        </Button>
      </div>
      <div>
        <CopyButton text={report.text} label="Copy report" />
      </div>
    </section>
  );
}

function names(apps: string[]) {
  return apps.map((app, i) => (
    <span key={app}>
      {i > 0 && ", "}
      <AppLink name={app} />
    </span>
  ));
}

function AppLink({ name }: { name: string }) {
  return (
    <Link to="/apps/$name" params={{ name }} className="font-medium text-accent underline">
      {name}
    </Link>
  );
}
