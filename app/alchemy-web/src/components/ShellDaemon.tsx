import { Play } from "lucide-react";
import type { ReactNode } from "react";

import { startDaemon, useShellLaunch } from "../lib/shell.ts";
import { Button } from "./Button.tsx";
import { Notice } from "./Notice.tsx";

/**
 * Shown in the desktop shell while alchemyd doesn't answer: the shell
 * starting it, why it couldn't, or a button to start it again. Falls back
 * to `fallback`, the browser's banner, while the shell is only waiting for
 * alchemyd to come back or can't say.
 */
export function ShellDaemon({ fallback }: { fallback: ReactNode }) {
  const launch = useShellLaunch(true);
  switch (launch?.kind) {
    case "checking":
    case "starting":
      return (
        <div role="status" className="shrink-0 border-b border-border bg-surface-muted px-6 py-3">
          <Notice tone="busy" title="Starting Alchemy…">
            <p>This takes a few seconds the first time.</p>
          </Notice>
        </div>
      );
    case "failed":
      return (
        <Banner title="Alchemy couldn't start alchemyd" action={<StartButton label="Try again" />}>
          {launch.message}
        </Banner>
      );
    case "stopped":
      return (
        <Banner title="alchemyd stopped" action={<StartButton label="Start Alchemy" />}>
          It didn't come back on its own.
        </Banner>
      );
    case "unavailable":
      return <Banner title="Alchemy can't start alchemyd from here">{launch.message}</Banner>;
    default:
      return fallback;
  }
}

function Banner({
  title,
  action,
  children,
}: {
  title: string;
  action?: ReactNode;
  children: ReactNode;
}) {
  return (
    <div role="alert" className="shrink-0 border-b border-border bg-error/10 px-6 py-3">
      <Notice tone="error" title={title} action={action}>
        <p>{children}</p>
      </Notice>
    </div>
  );
}

function StartButton({ label }: { label: string }) {
  return (
    <Button onClick={() => void startDaemon()}>
      <Play className="size-4" aria-hidden />
      {label}
    </Button>
  );
}
