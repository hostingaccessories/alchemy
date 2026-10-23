import { createRoute } from "@tanstack/react-router";
import { Boxes } from "lucide-react";

import { PageHeader } from "../components/PageHeader.tsx";
import { rootRoute } from "./root.tsx";

/** The apps on this machine. Nothing to list until apps can be added. */
export const appsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/apps",
  component: AppsPage,
});

function AppsPage() {
  return (
    <div className="flex flex-col items-center gap-2 py-16 text-center">
      <PageHeader title="Apps" />
      <Boxes className="size-10 text-stone-600" aria-hidden />
      <h2 className="text-lg font-semibold">No apps yet</h2>
      <p className="max-w-sm text-stone-400">Apps you add to Alchemy will show up here.</p>
    </div>
  );
}
