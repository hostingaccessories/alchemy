import { ArrowDown } from "lucide-react";
import { useEffect, useLayoutEffect, useRef, useState } from "react";

import type { App, LogLine } from "../lib/api.ts";
import { openLogs } from "../lib/logs.ts";
import { Button } from "./Button.tsx";
import { CopyButton } from "./CopyButton.tsx";
import { Loading } from "./Loading.tsx";
import { Notice } from "./Notice.tsx";

/** The most lines kept on screen; older ones are dropped as new ones arrive. */
export const MAX_LINES = 5000;

/** How close to the bottom, in pixels, still counts as reading the newest line. */
const BOTTOM_SLACK = 16;

type Phase =
  | { kind: "loading" }
  | { kind: "following" }
  | { kind: "done" }
  | { kind: "reconnecting" }
  | { kind: "failed"; message: string };

interface Shown {
  id: number;
  line: LogLine;
}

const CONTROL =
  "rounded-md border border-border bg-surface px-2 py-1.5 text-sm focus-visible:ring-2 focus-visible:ring-accent focus-visible:outline-none";

/**
 * The app's logs: the last lines of every container, or one service's, then
 * new lines as they arrive while "Follow new lines" is on. The view keeps
 * to the newest line until the reader scrolls up, and offers a way back.
 */
export function AppLogs({ app }: { app: App }) {
  const [service, setService] = useState("");
  const [follow, setFollow] = useState(true);
  const [attempt, setAttempt] = useState(0);
  const [lines, setLines] = useState<Shown[]>([]);
  const [phase, setPhase] = useState<Phase>({ kind: "loading" });
  const [pinned, setPinned] = useState(true);
  const view = useRef<HTMLDivElement>(null);
  const nextId = useRef(0);

  useEffect(() => {
    setLines([]);
    setPhase({ kind: "loading" });
    setPinned(true);
    const query = { service: service === "" ? undefined : service, follow };
    return openLogs(app.name, query, {
      onOpen: () => setLines([]),
      onEvent: (event) => {
        switch (event.event) {
          case "log": {
            const shown = { id: nextId.current++, line: event.data };
            setLines((current) => [...current.slice(-(MAX_LINES - 1)), shown]);
            break;
          }
          case "following":
            setPhase({ kind: "following" });
            break;
          case "done":
            setPhase(
              event.data.error === null
                ? { kind: "done" }
                : { kind: "failed", message: event.data.error },
            );
            break;
        }
      },
      onError: (retrying) =>
        setPhase(
          retrying
            ? { kind: "reconnecting" }
            : { kind: "failed", message: "Alchemy couldn't read this app's logs." },
        ),
    });
  }, [app.name, service, follow, attempt]);

  useLayoutEffect(() => {
    const el = view.current;
    if (pinned && el) {
      el.scrollTop = el.scrollHeight;
    }
  }, [lines, pinned]);

  const onScroll = () => {
    const el = view.current;
    if (el) {
      setPinned(el.scrollHeight - el.scrollTop - el.clientHeight <= BOTTOM_SLACK);
    }
  };

  const showService = service === "" && app.services.length > 1;
  const hasContainers = app.services.some((s) => s.containers.length > 0);
  const text = lines
    .map(({ line }) => (showService ? `${line.service} | ${line.line}` : line.line))
    .join("\n");

  return (
    <section aria-labelledby="logs" className="flex flex-col gap-3">
      <h2 id="logs" className="text-base font-semibold">
        Logs
      </h2>
      <div className="flex flex-wrap items-center gap-4">
        <label className="flex items-center gap-2 text-sm">
          Service
          <select className={CONTROL} value={service} onChange={(e) => setService(e.target.value)}>
            <option value="">All services</option>
            {app.services.map((s) => (
              <option key={s.name} value={s.name}>
                {s.name}
              </option>
            ))}
          </select>
        </label>
        <label className="flex items-center gap-2 text-sm">
          <input
            type="checkbox"
            className="size-4 accent-accent"
            checked={follow}
            onChange={(e) => setFollow(e.target.checked)}
          />
          Follow new lines
        </label>
        <span className="ml-auto">
          <CopyButton text={text} label="Copy logs" />
        </span>
      </div>
      {phase.kind === "failed" && (
        <div role="alert">
          <Notice
            tone="error"
            title="Couldn't read the logs"
            action={<Button onClick={() => setAttempt((n) => n + 1)}>Try again</Button>}
          >
            <p>{phase.message}</p>
          </Notice>
        </div>
      )}
      {phase.kind === "reconnecting" && (
        <p role="status" className="text-sm text-muted">
          Lost the connection to alchemyd. Reconnecting…
        </p>
      )}
      {phase.kind === "loading" && lines.length === 0 ? (
        <Loading label="Loading logs…" />
      ) : (
        <div className="relative">
          <div
            ref={view}
            role="log"
            aria-label="Log lines"
            tabIndex={0}
            onScroll={onScroll}
            className="max-h-96 min-h-24 overflow-auto rounded-lg border border-border bg-surface-muted p-3 font-mono text-xs leading-5"
          >
            {lines.length === 0 ? (
              <p className="font-sans text-sm text-muted">
                {!hasContainers
                  ? "This app has no containers yet, so there's nothing to show."
                  : phase.kind === "following"
                    ? "No log lines yet. New ones appear here as they're written."
                    : "No log lines."}
              </p>
            ) : (
              lines.map(({ id, line }) => (
                <div
                  key={id}
                  title={line.time ?? undefined}
                  className="break-all whitespace-pre-wrap"
                >
                  {showService && <span className="text-muted">{line.service} | </span>}
                  {line.line}
                </div>
              ))
            )}
          </div>
          {!pinned && lines.length > 0 && (
            <Button className="absolute right-3 bottom-3 shadow" onClick={() => setPinned(true)}>
              <ArrowDown className="size-4" aria-hidden />
              Jump to newest
            </Button>
          )}
        </div>
      )}
    </section>
  );
}
