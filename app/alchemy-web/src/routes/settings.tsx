import { createRoute } from "@tanstack/react-router";

import { PageHeader } from "../components/PageHeader.tsx";
import { rootRoute } from "./root.tsx";

/** Per-client preferences. The theme picker lands in #39. */
export const settingsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/settings",
  component: SettingsPage,
});

function SettingsPage() {
  return (
    <section aria-labelledby="appearance" className="flex max-w-xl flex-col gap-3">
      <PageHeader title="Settings" />
      <h2 id="appearance" className="text-base font-semibold">
        Appearance
      </h2>
      <p className="text-stone-400">Theme options are coming soon.</p>
    </section>
  );
}
