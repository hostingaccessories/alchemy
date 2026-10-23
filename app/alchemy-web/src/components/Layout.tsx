import { Link, Outlet } from "@tanstack/react-router";
import { Boxes, FlaskConical, type LucideIcon, Server, Settings } from "lucide-react";
import { useState } from "react";

import { useDaemonConnection } from "../lib/live.tsx";
import { DaemonStatus } from "./DaemonStatus.tsx";
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
 * state sits beside it. Fits the desktop shell's 800px minimum width.
 */
export function Layout() {
  const [slot, setSlot] = useState<HTMLElement | null>(null);
  const daemon = useDaemonConnection();

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
        <main className="min-h-0 flex-1 overflow-y-auto px-6 py-6">
          <HeaderSlotContext value={slot}>
            <Outlet />
          </HeaderSlotContext>
        </main>
      </div>
    </div>
  );
}
