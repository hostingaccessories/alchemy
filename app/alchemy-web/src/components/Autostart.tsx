import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import {
  ApiError,
  type Autostart,
  type AutostartAttempt,
  type AutostartChange,
  type AutostartMode,
  type LogHint,
  type Supervisor,
  api,
} from "../lib/api.ts";
import { useSignedOut } from "../lib/live.tsx";
import { liveQueries } from "../lib/queries.ts";
import { expectRestart } from "../lib/restart.ts";
import { Code } from "./Code.tsx";
import { CopyButton } from "./CopyButton.tsx";
import { Notice } from "./Notice.tsx";
import { TerminalSteps } from "./Privileges.tsx";

/** Each mode's choice, in the Novice's words. */
const MODES: Record<AutostartMode, { label: string; what: string }> = {
  off: {
    label: "Don't start on its own",
    what: "Start alchemyd yourself, or open the Alchemy app.",
  },
  login: {
    label: "Start at login",
    what: "No password needed. alchemyd starts when you log in to this computer, and Docker Desktop with it.",
  },
  boot: {
    label: "Start at boot",
    what: "When this machine starts, before anyone logs in, for a home server. Needs an administrator's password once.",
  },
};

const RUNNING: Record<Supervisor, string> = {
  manual: "alchemyd was started by hand this time.",
  user_service: "alchemyd is running as the service that starts at login.",
  system_service: "alchemyd is running as the service that starts at boot.",
};

/** How the last switch ended, when it didn't work. */
function lastAttempt(last: AutostartAttempt | null): string | null {
  if (last === null) {
    return null;
  }
  const what = `The last change to “${MODES[last.mode]?.label ?? last.mode}”`;
  switch (last.result) {
    case "cancelled":
      return `${what} was cancelled at the system prompt.`;
    case "denied":
      return `${what} wasn't allowed: the system prompt didn't accept the password.`;
    case "failed":
      return last.message === null ? `${what} failed.` : `${what} failed: ${last.message}`;
    default:
      return null;
  }
}

/**
 * Whether alchemyd starts on its own: off, at login, or at boot. Changing
 * it saves at once. Starting at boot goes through the system prompt, or a
 * command to copy when there's none, like the privileges. Says where
 * alchemyd logs. Renders nothing when alchemyd is too old to report it, or
 * while signed out.
 */
export function AutostartSection() {
  const client = useQueryClient();
  const autostart = useQuery(liveQueries.autostart);
  const signedOut = useSignedOut();
  const save = useMutation({
    mutationKey: ["autostart"],
    mutationFn: (mode: AutostartMode) => api.setAutostart({ mode }),
    onSuccess: (change) => {
      client.setQueryData(liveQueries.autostart.queryKey, change.autostart);
      if (change.outcome === "restarting") {
        expectRestart(client);
      }
    },
  });
  const value = autostart.data;
  if (signedOut || value === undefined || !Array.isArray(value.available_modes)) {
    return null;
  }

  const answer: AutostartChange | undefined = save.isSuccess ? save.data : undefined;
  const asked = save.variables;
  const terminal =
    answer?.outcome === "needs_terminal" &&
    answer.command !== null &&
    asked !== undefined &&
    value.pending === null &&
    value.mode !== asked
      ? { mode: asked, command: answer.command }
      : null;
  const chosen = save.isPending ? save.variables : (value.pending ?? terminal?.mode ?? value.mode);
  const busy = save.isPending || value.pending !== null;
  const tried = value.pending === null ? lastAttempt(value.last) : null;

  return (
    <section aria-labelledby="autostart" className="flex max-w-xl flex-col gap-3">
      <h2 id="autostart" className="text-base font-semibold">
        Start automatically
      </h2>
      <p className="text-sm text-muted">
        Alchemy brings your apps back while alchemyd runs. Choose when it starts on its own.
      </p>
      <fieldset className="flex flex-col gap-2">
        <legend className="sr-only">When alchemyd starts</legend>
        {value.available_modes.map((mode) => (
          <label key={mode} className="flex items-start gap-2 text-sm">
            <input
              type="radio"
              name="autostart"
              value={mode}
              className="mt-0.5 size-4 accent-accent"
              checked={chosen === mode}
              disabled={busy}
              onChange={() => save.mutate(mode)}
            />
            <span className="flex flex-col gap-0.5">
              <span className="font-medium">{MODES[mode]?.label ?? mode}</span>
              <span className="text-muted">{MODES[mode]?.what}</span>
            </span>
          </label>
        ))}
      </fieldset>
      {(value.unavailable ?? []).length > 0 && (
        <ul aria-label="Not available here" className="flex flex-col gap-1 text-sm text-muted">
          {(value.unavailable ?? []).map(({ mode, message }) => (
            <li key={mode}>
              <span className="font-medium text-text">{MODES[mode]?.label ?? mode}</span> isn't
              available here: {message}
            </li>
          ))}
        </ul>
      )}
      <div role="status" className="flex flex-col gap-2">
        {value.pending !== null && (
          <Notice tone="busy" title="Waiting for you to answer the system prompt…" />
        )}
        {answer?.outcome === "restarting" && (
          <Notice tone="busy" title="Handing over to the service that starts at boot…">
            <p>alchemyd stops in a moment and the service takes over. Alchemy reconnects.</p>
          </Notice>
        )}
        {tried !== null && <p className="text-sm text-muted">{tried}</p>}
      </div>
      {terminal !== null && (
        <div className="flex flex-col gap-1 text-sm text-muted">
          {answer?.message && <p>{answer.message}</p>}
          <TerminalSteps
            command={terminal.command}
            again={false}
            sending={save.isPending}
            onTryAgain={() => save.mutate(terminal.mode)}
          />
        </div>
      )}
      {save.isError && (
        <div role="alert">
          <Notice tone="error" title="Alchemy couldn't change when alchemyd starts">
            <p>
              {save.error instanceof ApiError
                ? save.error.message
                : "Something went wrong. Try again."}
            </p>
          </Notice>
        </div>
      )}
      <div className="flex flex-col gap-1 text-sm text-muted">
        <p>{RUNNING[value.supervisor] ?? RUNNING.manual}</p>
        <Logs hint={value.log_hint} />
      </div>
    </section>
  );
}

/** Where alchemyd's logs are, to read when something goes wrong. */
function Logs({ hint }: { hint: Autostart["log_hint"] }) {
  switch ((hint as LogHint | undefined)?.kind) {
    case "file": {
      const { path } = hint as Extract<LogHint, { kind: "file" }>;
      return (
        <p>
          Its log is <Code>{path}</Code>.
        </p>
      );
    }
    case "journal": {
      const { command } = hint as Extract<LogHint, { kind: "journal" }>;
      return (
        <>
          <p>
            To read its log, run <Code>{command}</Code> in a terminal.
          </p>
          <div>
            <CopyButton text={command} label="Copy command" />
          </div>
        </>
      );
    }
    default:
      return <p>Its log is in the terminal it was started from.</p>;
  }
}
