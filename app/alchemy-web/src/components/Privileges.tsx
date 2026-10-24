import {
  type MutationState,
  useMutation,
  useMutationState,
  useQuery,
  useQueryClient,
} from "@tanstack/react-query";
import type { ReactNode } from "react";

import {
  ApiError,
  type HelperCommand,
  type Privilege,
  type PrivilegeAction,
  type PrivilegeAttempt,
  type PrivilegeChange,
  type PrivilegeState,
  api,
} from "../lib/api.ts";
import { useSignedOut } from "../lib/live.tsx";
import { isTauri } from "../lib/platform.ts";
import { liveQueries } from "../lib/queries.ts";
import { expectRestart, useRestart } from "../lib/restart.ts";
import { Button } from "./Button.tsx";
import { Code } from "./Code.tsx";
import { CopyButton } from "./CopyButton.tsx";
import { Notice, type Tone } from "./Notice.tsx";

/** The words for one action: what it is for, its buttons, and its states. */
interface ActionText {
  /** What it does for the user, in a sentence. */
  what: string;
  grant: string;
  undo: string;
  /** A title for each state the daemon can report, problem first. */
  titles: Record<"granted" | "not_needed" | "needed" | "needs_restart" | "unsupported", string>;
}

const ACTIONS: Record<PrivilegeAction, ActionText> = {
  low_ports: {
    what: "Port 80 serves addresses like http://<app>.localhost and http://<app>.local.",
    grant: "Allow port 80",
    undo: "Remove port 80 access",
    titles: {
      granted: "Alchemy can use port 80",
      not_needed: "Alchemy can use port 80 without extra permission",
      needed: "Alchemy needs permission to use port 80",
      needs_restart: "Port 80 is allowed once alchemyd restarts",
      unsupported: "Alchemy can't be allowed port 80 on this machine",
    },
  },
  firewall: {
    what: "The firewall decides whether other devices on your network can reach your apps on port 80, their own ports, and .local names.",
    grant: "Open the firewall",
    undo: "Close the firewall",
    titles: {
      granted: "The firewall lets your network reach your apps",
      not_needed: "No firewall needs opening",
      needed: "The firewall may block other devices from your apps",
      needs_restart: "The firewall is open once alchemyd restarts",
      unsupported: "Alchemy can't open this firewall",
    },
  },
  system_service: {
    what: "A system service starts alchemyd when this machine starts, before anyone signs in.",
    grant: "Install the service",
    undo: "Remove the service",
    titles: {
      granted: "alchemyd runs as a system service",
      not_needed: "alchemyd doesn't need a system service",
      needed: "alchemyd needs a system service",
      needs_restart: "The service is in place once alchemyd restarts",
      unsupported: "Alchemy can't install a system service on this machine",
    },
  },
  other: {
    what: "This version of Alchemy doesn't recognize this privilege. Update Alchemy.",
    grant: "Grant",
    undo: "Undo",
    titles: {
      granted: "A privilege this version of Alchemy doesn't recognize is granted",
      not_needed: "A privilege this version of Alchemy doesn't recognize isn't needed",
      needed: "A privilege this version of Alchemy doesn't recognize is needed",
      needs_restart: "A privilege this version of Alchemy doesn't recognize needs a restart",
      unsupported: "A privilege this version of Alchemy doesn't recognize isn't supported",
    },
  },
};

function text(action: PrivilegeAction): ActionText {
  return ACTIONS[action] ?? ACTIONS.other;
}

/** A grant (`true`) or undo (`false`) sent for one action, and how it went. */
type Change = MutationState<PrivilegeChange, Error, boolean>;

/**
 * One action from the `privileges` live query, and the way to grant or undo
 * it. Every component that shows the same action shares its requests through
 * the mutation cache, so the port 80 warning and the privileges section stay
 * in step. A `restarting` answer is noted for the layout, which then says
 * alchemyd is restarting instead of unreachable.
 */
export function usePrivilege(action: PrivilegeAction) {
  const client = useQueryClient();
  const privileges = useQuery(liveQueries.privileges);
  const mutationKey = ["privilege", action];
  const mutation = useMutation({
    mutationKey,
    mutationFn: (grant: boolean) => api.changePrivilege(action, { grant }),
    onSuccess: (change) => {
      if (change.outcome === "restarting") {
        expectRestart(client);
      }
    },
  });
  const changes = useMutationState({
    filters: { mutationKey },
    select: (m) => m.state as Change,
  });
  return {
    privilege: privileges.data?.privileges.find((p) => p.action === action),
    change: (grant: boolean) => mutation.mutate(grant),
    changes,
  };
}

/**
 * The command to run in a terminal, if the last request answered
 * `needs_terminal` for the direction the action still needs, or else the
 * one the daemon reports for granting it.
 */
function terminalFor(
  privilege: Privilege,
  last: Change | undefined,
): { grant: boolean; command: HelperCommand } | null {
  if (privilege.state === "pending") {
    return null;
  }
  const answer = last?.status === "success" ? last.data : undefined;
  if (answer?.outcome === "needs_terminal" && answer.command !== null) {
    const grant = last?.variables ?? true;
    const open: PrivilegeState[] = grant
      ? ["needed", "needs_terminal"]
      : ["granted", "needs_restart"];
    if (open.includes(privilege.state)) {
      return { grant, command: answer.command };
    }
  }
  if (privilege.state === "needs_terminal" && privilege.command !== null) {
    return { grant: true, command: privilege.command };
  }
  return null;
}

/** How many requests in a row, ending with the latest, answered `needs_terminal`. */
function terminalAnswers(changes: Change[]): number {
  let count = 0;
  for (let i = changes.length - 1; i >= 0; i--) {
    if (changes[i]?.data?.outcome !== "needs_terminal") break;
    count++;
  }
  return count;
}

function tone(state: PrivilegeState): Tone {
  switch (state) {
    case "granted":
    case "not_needed":
      return "ok";
    case "pending":
      return "busy";
    default:
      return "warn";
  }
}

function title(privilege: Privilege, restarting: boolean): string {
  const titles = text(privilege.action).titles;
  switch (privilege.state) {
    case "pending":
      return restarting
        ? "Restarting alchemyd to put the change in effect…"
        : "Waiting for you to answer the system prompt…";
    case "needs_terminal":
      return titles.needed;
    case "granted":
    case "not_needed":
    case "needed":
    case "needs_restart":
    case "unsupported":
      return titles[privilege.state];
    default:
      return "This privilege is in a state this version of Alchemy doesn't recognize. Update Alchemy.";
  }
}

/** How the last grant or undo through this alchemyd ended, when it didn't work. */
function lastAttempt(last: PrivilegeAttempt | null): string | null {
  if (last === null) {
    return null;
  }
  const what = last.grant ? "The last try" : "The last undo";
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
 * One privilege, problem first, with what to do next: the button that asks
 * for the system prompt, the prompt's progress, the command to copy when
 * there is no prompt to show, or why it can't be done. `title` and `intro`
 * replace the action's own, as the port 80 warning does. Without `undo`
 * there is no button to undo a granted action.
 */
export function PrivilegeNotice({
  action,
  title: titleOverride,
  intro,
  undo = true,
}: {
  action: PrivilegeAction;
  title?: string;
  intro?: ReactNode;
  undo?: boolean;
}) {
  const { privilege, change, changes } = usePrivilege(action);
  const { restarting: expected } = useRestart();
  if (privilege === undefined) {
    return null;
  }
  const words = text(action);
  const latest = changes.at(-1);
  const sending = latest?.status === "pending";
  const restarting =
    privilege.state === "pending" &&
    (expected || (privilege.restarts && privilege.last?.result === "done"));
  const terminal = terminalFor(privilege, latest);
  const failure = latest?.status === "error" ? latest.error : null;
  const tried = lastAttempt(privilege.state === "pending" ? null : privilege.last);
  const message =
    privilege.state === "needs_terminal" || privilege.state === "unsupported"
      ? privilege.message
      : terminal !== null
        ? (latest?.data?.message ?? null)
        : null;
  const offers =
    privilege.state === "needed" ||
    privilege.state === "needs_restart" ||
    (undo && privilege.state === "granted");

  const buttons: ReactNode[] = [];
  if (terminal === null && privilege.state === "needed") {
    buttons.push(
      <Button key="grant" variant="primary" disabled={sending} onClick={() => change(true)}>
        {words.grant}
      </Button>,
    );
  }
  if (privilege.state === "needs_restart") {
    buttons.push(
      <Button key="restart" variant="primary" disabled={sending} onClick={() => change(true)}>
        Restart alchemyd
      </Button>,
    );
  }
  if (terminal === null && undo && privilege.state === "granted") {
    buttons.push(
      <Button key="undo" disabled={sending} onClick={() => change(false)}>
        {words.undo}
      </Button>,
    );
  }

  return (
    <Notice
      tone={privilege.state === "pending" ? "busy" : titleOverride ? "warn" : tone(privilege.state)}
      title={
        privilege.state === "pending" || !titleOverride
          ? title(privilege, restarting)
          : titleOverride
      }
      action={buttons.length > 0 ? buttons : undefined}
    >
      {intro ?? (titleOverride === undefined && <p>{words.what}</p>)}
      {message !== null && <p>{message}</p>}
      {offers && privilege.restarts && terminal === null && <RestartNote />}
      {tried !== null && <p>{tried}</p>}
      {terminal !== null && (
        <TerminalSteps
          command={terminal.command}
          again={terminalAnswers(changes) > 1}
          sending={sending}
          onTryAgain={() => change(terminal.grant)}
        />
      )}
      {failure !== null && (
        <div role="alert">
          <Notice tone="error" title="Alchemy couldn't make the change">
            <p>
              {failure instanceof ApiError ? failure.message : "Something went wrong. Try again."}
            </p>
          </Notice>
        </div>
      )}
    </Notice>
  );
}

/** What a restart of alchemyd means here: a moment offline, and in a browser, signing in again. */
function RestartNote() {
  return isTauri() ? (
    <p>alchemyd restarts to put this in effect, and Alchemy reconnects on its own.</p>
  ) : (
    <p>
      alchemyd restarts to put this in effect, which signs this browser out: run{" "}
      <Code>alchemy open</Code> to sign in again.
    </p>
  );
}

/**
 * The command to run when there's no system prompt to show, and the button
 * that asks alchemyd to check again once it has run.
 */
function TerminalSteps({
  command,
  again,
  sending,
  onTryAgain,
}: {
  command: HelperCommand;
  again: boolean;
  sending: boolean;
  onTryAgain: () => void;
}) {
  return (
    <>
      <p>Run this command in a terminal on this machine, then select Try again:</p>
      <pre className="overflow-x-auto rounded bg-surface-muted p-2 text-xs text-text">
        {command.line}
      </pre>
      {again && (
        <p>
          Alchemy doesn't see the change yet. Check that the command finished without an error, then
          try again.
        </p>
      )}
      <div className="flex flex-wrap items-center gap-2">
        <CopyButton text={command.line} label="Copy command" />
        <Button disabled={sending} onClick={onTryAgain}>
          Try again
        </Button>
      </div>
    </>
  );
}

/**
 * The changes to this machine that need an administrator (port 80, the
 * firewall, a system service), each with its state and what to do next.
 * Renders nothing when alchemyd is too old to report them, or while signed
 * out, when what the page last saw may be from before a restart. Actions
 * this version doesn't know are left out, since it couldn't change them.
 */
export function PrivilegesSection() {
  const privileges = useQuery(liveQueries.privileges);
  const signedOut = useSignedOut();
  if (signedOut || privileges.data === undefined) {
    return null;
  }
  return (
    <section aria-labelledby="privileges" className="flex max-w-xl flex-col gap-3">
      <h2 id="privileges" className="text-base font-semibold">
        Privileges
      </h2>
      <p className="text-sm text-muted">
        Some changes to this machine need an administrator. Alchemy asks for your password with the
        system's own prompt, once for each.
      </p>
      <ul className="flex flex-col divide-y divide-border">
        {privileges.data.privileges
          .filter((p) => p.action !== "other")
          .map((p) => (
            <li
              key={p.action}
              role="group"
              aria-label={privilegeName(p.action)}
              className="py-3 first:pt-0 last:pb-0"
            >
              <PrivilegeNotice action={p.action} />
            </li>
          ))}
      </ul>
    </section>
  );
}

/** The action's name, for a label. */
export function privilegeName(action: PrivilegeAction): string {
  switch (action) {
    case "low_ports":
      return "Port 80";
    case "firewall":
      return "Firewall";
    case "system_service":
      return "System service";
    default:
      return "Unknown privilege";
  }
}
