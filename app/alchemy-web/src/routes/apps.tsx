import { createRoute } from "@tanstack/react-router";
import { Boxes } from "lucide-react";

import { EmptyState } from "../components/EmptyState.tsx";
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
    <>
      <PageHeader title="Apps" />
      <EmptyState icon={Boxes} title="No apps yet">
        Apps you add to Alchemy will show up here.
      </EmptyState>
    </>
  );
}
