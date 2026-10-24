import { createRoute } from "@tanstack/react-router";
import { type LucideIcon, Monitor, Moon, Sun } from "lucide-react";

import { AppFolders } from "../components/AppFolders.tsx";
import { PageHeader } from "../components/PageHeader.tsx";
import { type ThemePreference, THEME_PREFERENCES, useTheme } from "../lib/theme.ts";
import { rootRoute } from "./root.tsx";

/**
 * Where alchemyd looks for apps, kept in the daemon, and per-client
 * preferences, kept in this browser.
 */
export const settingsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/settings",
  component: SettingsPage,
});

const THEME_OPTIONS: Record<ThemePreference, { label: string; icon: LucideIcon }> = {
  system: { label: "System", icon: Monitor },
  light: { label: "Light", icon: Sun },
  dark: { label: "Dark", icon: Moon },
};

function SettingsPage() {
  return (
    <div className="flex max-w-xl flex-col gap-8">
      <PageHeader title="Settings" />
      <section aria-labelledby="app-folders" className="flex flex-col gap-3">
        <h2 id="app-folders" className="text-base font-semibold">
          App folders
        </h2>
        <AppFolders />
      </section>
      <section aria-labelledby="appearance" className="flex flex-col gap-3">
        <h2 id="appearance" className="text-base font-semibold">
          Appearance
        </h2>
        <ThemePicker />
      </section>
    </div>
  );
}

function ThemePicker() {
  const { preference, theme, setPreference } = useTheme();

  return (
    <fieldset className="flex flex-col gap-2">
      <legend className="mb-2 text-sm text-muted">Theme</legend>
      <div className="flex w-fit gap-1 rounded-lg border border-border bg-surface-raised p-1">
        {THEME_PREFERENCES.map((value) => {
          const { label, icon: Icon } = THEME_OPTIONS[value];
          return (
            <label
              key={value}
              className="flex cursor-pointer items-center gap-2 rounded-md px-3 py-1.5 text-sm text-muted hover:text-text has-checked:bg-surface-muted has-checked:text-text has-focus-visible:outline-2 has-focus-visible:outline-accent"
            >
              <input
                type="radio"
                name="theme"
                value={value}
                checked={preference === value}
                onChange={() => setPreference(value)}
                className="sr-only"
              />
              <Icon className="size-4" aria-hidden />
              {label}
            </label>
          );
        })}
      </div>
      <p className="text-sm text-muted">
        {preference === "system" && `Following your system, which is ${theme} right now. `}
        Saved in this browser only.
      </p>
    </fieldset>
  );
}
