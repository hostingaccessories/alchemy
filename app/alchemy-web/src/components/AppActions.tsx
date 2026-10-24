import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useState } from "react";

import {
  type Action,
  ApiError,
  type App,
  type Operation,
  type OutputLine,
  type PortConflict,
  api,
} from "../lib/api.ts";
import { liveQueries, showOperation } from "../lib/queries.ts";
import { Advanced } from "./Advanced.tsx";
import { Button } from "./Button.tsx";
import { ConfirmDialog } from "./ConfirmDialog.tsx";
import { Notice } from "./Notice.tsx";
import { ProgressSteps, type Step, type StepState } from "./ProgressSteps.tsx";

/** What each action's button says. */
const BUTTONS: Record<Action, string> = {
  start: "Start",
  stop: "Stop",
  restart: "Restart",
  update: "Update",
  up: "Create and start",
  pull: "Download images",
  build: "Build images",
  down: "Take down",
};

/** What a running operation is doing, for its heading. */
const RUNNING: Record<Action, string> = {
  start: "Starting",
  stop: "Stopping",
  restart: "Restarting",
  update: "Updating",
  up: "Creating and starting",
  pull: "Downloading images for",
  build: "Building images for",
  down: "Taking down",
};

/** What a finished operation did, for its heading. */
const DONE: Record<Action, string> = {
  start: "Started",
  stop: "Stopped",
  restart: "Restarted",
  update: "Updated",
  up: "Created and started",
  pull: "Downloaded images for",
  build: "Built images for",
  down: "Took down",
};

const PRIMARY: Action[] = ["start", "stop", "restart", "update"];

/** The actions alchemyd checks the app's host ports for before running. */
const PORT_CHECKED: ReadonlySet<Action> = new Set(["up", "start", "restart", "update"]);

/** An action refused because ports it needs are taken. */
interface PortRefusal {
  action: Action;
  conflicts: PortConflict[];
}

/**
 * An app's lifecycle actions and the progress of its latest one. Start,
 * Stop, Restart, and Update are always shown; the rest sit under More actions,
 * and Take down asks first. Only one action runs on an app at a time, so
 * the buttons wait while one does. Progress comes from the live
 * `operations` event, whichever client started it; raw output from the
 * operation's own stream, folded under each step. An action refused because
 * the app's ports are taken offers Start anyway under Advanced.
 */
export function AppActions({ app }: { app: App }) {
  const client = useQueryClient();
  const operations = useQuery(liveQueries.operations);
  const latest = operations.data?.operations.find((op) => op.app === app.name);
  const running = latest?.state === "running";
  const [error, setError] = useState<string | null>(null);
  const [portRefusal, setPortRefusal] = useState<PortRefusal | null>(null);

  const start = useMutation({
    mutationFn: ({ action, force }: { action: Action; force?: boolean }) =>
      api.startAction(app.name, force ? { action, force } : { action }),
    onMutate: () => {
      setError(null);
      setPortRefusal(null);
    },
    onSuccess: ({ operation }) => showOperation(client, operation),
    onError: async (err, { action }) => {
      setError(err instanceof ApiError ? err.message : "Something went wrong.");
      if (err instanceof ApiError && err.code === "conflict" && PORT_CHECKED.has(action)) {
        const plan = await api.actionPlan(app.name, action).catch(() => null);
        const conflicts = plan?.conflicts ?? [];
        if (conflicts.length > 0) setPortRefusal({ action, conflicts });
      }
    },
  });

  const unreadable = app.project === null;
  const busy = running || start.isPending;
  const disabled = unreadable || busy;
  const run = (action: Action) => start.mutate({ action });

  return (
    <section aria-labelledby="actions" className="flex flex-col gap-3">
      <h2 id="actions" className="text-base font-semibold">
        Actions
      </h2>
      {unreadable && (
        <p className="text-sm text-muted">
          Alchemy can run this app once Compose can read its files.
        </p>
      )}
      <div className="flex flex-wrap gap-2">
        {PRIMARY.map((action) => (
          <Button
            key={action}
            variant={action === "start" ? "primary" : "secondary"}
            disabled={disabled}
            onClick={() => run(action)}
          >
            {BUTTONS[action]}
          </Button>
        ))}
      </div>
      <Advanced label="More actions">
        <div className="flex flex-wrap gap-2">
          {(["up", "pull", "build"] as const).map((action) => (
            <Button key={action} disabled={disabled} onClick={() => run(action)}>
              {BUTTONS[action]}
            </Button>
          ))}
          <ConfirmDialog
            trigger={
              <Button variant="danger" disabled={disabled}>
                {BUTTONS.down}
              </Button>
            }
            title={`Take ${app.name} down?`}
            description={
              <p>
                Its containers and networks are removed. Its data volumes and files are kept, and
                Create and start brings it back.
              </p>
            }
            confirmLabel="Take down"
            onConfirm={async () => {
              await start.mutateAsync({ action: "down" });
            }}
          />
        </div>
      </Advanced>
      {error && (
        <div role="alert">
          <Notice tone="error" title={error}>
            {portRefusal && (
              <Advanced>
                <p>
                  Start anyway runs it regardless. Docker still refuses a port that is really in
                  use, so the app may not start.
                </p>
                <div>
                  <Button
                    disabled={disabled}
                    onClick={() => start.mutate({ action: portRefusal.action, force: true })}
                  >
                    Start anyway
                  </Button>
                </div>
              </Advanced>
            )}
          </Notice>
        </div>
      )}
      {latest && <OperationProgress key={latest.id} operation={latest} />}
    </section>
  );
}

function heading(op: Operation): string {
  switch (op.state) {
    case "running":
      return `${RUNNING[op.action]} ${op.app}…`;
    case "succeeded":
      return `${DONE[op.action]} ${op.app}`;
    case "cancelled":
      return `${BUTTONS[op.action]} was cancelled`;
    case "interrupted":
      return `${BUTTONS[op.action]} stopped when alchemyd shut down`;
    default:
      return `${BUTTONS[op.action]} didn't finish`;
  }
}

function stepState(state: Operation["steps"][number]["state"]): StepState {
  return state === "unknown" ? "pending" : state;
}

/** One operation's steps in plain words, its raw output folded under each. */
function OperationProgress({ operation }: { operation: Operation }) {
  const output = useOperationOutput(operation.id);
  const [cancelError, setCancelError] = useState<string | null>(null);
  const steps: Step[] = operation.steps.map((step, index) => ({
    id: `${operation.id}-${index}`,
    label: step.state === "active" ? `${step.label}…` : step.label,
    state: stepState(step.state),
    message: step.message ?? undefined,
    output:
      output
        .filter((line) => line.step === index)
        .map((line) => line.text)
        .join("\n") || undefined,
  }));
  const tone =
    operation.state === "running" ? "busy" : operation.state === "succeeded" ? "ok" : "error";

  return (
    <div
      role="status"
      className="flex flex-col gap-3 rounded-lg border border-border bg-surface-raised p-4"
    >
      <Notice
        tone={tone}
        title={heading(operation)}
        action={
          operation.state === "running" ? (
            <Button
              onClick={() => {
                setCancelError(null);
                api
                  .cancelOperation(operation.id)
                  .catch((err: unknown) =>
                    setCancelError(err instanceof ApiError ? err.message : "Couldn't cancel."),
                  );
              }}
            >
              Cancel
            </Button>
          ) : undefined
        }
      >
        {operation.error && operation.state !== "running" && <p>{operation.error}</p>}
        {cancelError && <p className="text-error">{cancelError}</p>}
      </Notice>
      <ProgressSteps label={heading(operation)} steps={steps} />
    </div>
  );
}

/** The raw output of operation `id`, as far as the daemon has kept it. */
function useOperationOutput(id: string): OutputLine[] {
  const [lines, setLines] = useState<OutputLine[]>([]);
  useEffect(() => {
    setLines([]);
    return api.followOperation(id, (event) => {
      if (event.event === "output") {
        setLines((seen) => [...seen, event.data]);
      }
    });
  }, [id]);
  return lines;
}
