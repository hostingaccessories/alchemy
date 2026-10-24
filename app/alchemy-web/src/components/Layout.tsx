import { Link, Outlet } from "@tanstack/react-router";
import { Boxes, FlaskConical, type LucideIcon, Server, Settings } from "lucide-react";
import { type ReactNode, useState } from "react";

import type { ApiError } from "../lib/api.ts";
import { useDaemonConnection, useSignedOut } from "../lib/live.tsx";
import { isTauri } from "../lib/platform.ts";
import { useWindowTheme } from "../lib/theme.ts";
import { Code } from "./Code.tsx";
import { DaemonStatus } from "./DaemonStatus.tsx";
import { Notice } from "./Notice.tsx";
import { HeaderSlotContext } from "./PageHeader.tsx";

/** Sidebar entries, in the personas' words. */
const NAV: { to: "/apps" | "/machines" | "/settings"; label: string; icon: LucideIcon }[] = [
  { to: "/apps", label: "Apps", icon: Boxes },
  { to: "/machines", label: "Machines", icon: Server },
  { to: "/settings", label: "Settings", icon: Settings },
];

/**
 * The app frame: sidebar, header, and the current route in the outlet. Pages
 * fill the header's slot with {@link PageHeader}; the daemon's connection
 * state sits beside it, and a banner spans the page while the daemon is
 * unreachable. Fits the desktop shell's 800px minimum width.
 */
export function Layout() {
  const [slot, setSlot] = useState<HTMLElement | null>(null);
  const daemon = useDaemonConnection();
  const signedOut = useSignedOut();
  useWindowTheme();

  return (
    <div className="flex h-screen bg-surface text-text">
      <aside className="flex w-48 shrink-0 flex-col gap-6 border-r border-border px-3 py-4">
        <div className="flex items-center gap-2 px-2">
          <FlaskConical className="size-6 text-accent" aria-hidden />
          <span className="text-lg font-semibold tracking-tight">Alchemy</span>
        </div>
        <nav aria-label="Main">
          <ul className="flex flex-col gap-1">
            {NAV.map(({ to, label, icon: Icon }) => (
              <li key={to}>
                <Link
                  to={to}
                  className="flex items-center gap-2 rounded-md px-2 py-1.5 text-muted hover:bg-surface-muted hover:text-text"
                  activeProps={{ className: "bg-surface-muted font-medium text-text" }}
                >
                  <Icon className="size-4" aria-hidden />
                  {label}
                </Link>
              </li>
            ))}
          </ul>
        </nav>
      </aside>
      <div className="flex min-w-0 flex-1 flex-col">
        <header className="flex h-14 shrink-0 items-center gap-4 border-b border-border px-6">
          <div ref={setSlot} className="flex min-w-0 flex-1 items-center gap-4" />
          <div role="status" aria-label="alchemyd" className="shrink-0 text-sm">
            <DaemonStatus state={daemon} />
          </div>
        </header>
        {daemon.kind === "offline" && <DaemonUnreachable error={daemon.error} />}
        {daemon.kind === "online" && signedOut && <SignedOut />}
        <main className="min-h-0 flex-1 overflow-y-auto px-6 py-6">
          <HeaderSlotContext value={slot}>
            <Outlet />
          </HeaderSlotContext>
        </main>
      </div>
    </div>
  );
}

/**
 * Shown while `alchemyd` answers but refuses this page as not signed in. A
 * browser signs in through `alchemy open`; the shell signs in with the
 * daemon's token on its own, so there it means the token couldn't be read.
 * It clears itself once the event stream connects.
 */
function SignedOut() {
  const { title, body } = signedOutText();
  return (
    <div role="alert" className="shrink-0 border-b border-border bg-warn/10 px-6 py-3">
      <Notice tone="warn" title={title}>
        <p>{body}</p>
      </Notice>
    </div>
  );
}

/** What to say about being signed out, in the browser or the shell. */
function signedOutText(): { title: string; body: ReactNode } {
  if (isTauri()) {
    return {
      title: "Alchemy can't sign in to alchemyd",
      body: "Check that alchemyd is running as you on this machine. Alchemy tries again on its own.",
    };
  }
  return {
    title: "You're signed out of alchemyd",
    body: (
      <>
        Run <Code>alchemy open</Code> in a terminal on this machine to open Alchemy in your browser,
        signed in.
      </>
    ),
  };
}

/**
 * Shown while `alchemyd` doesn't answer, in the browser and the shell alike.
 * It clears itself once the event stream reconnects.
 */
function DaemonUnreachable({ error }: { error: ApiError }) {
  return (
    <div role="alert" className="shrink-0 border-b border-border bg-error/10 px-6 py-3">
      <Notice
        tone="error"
        title="Alchemy can't reach alchemyd"
        detail={<p className="text-sm text-muted">{error.message}</p>}
      >
        <p>Check that alchemyd is running on this machine. Alchemy reconnects on its own.</p>
      </Notice>
    </div>
  );
}
