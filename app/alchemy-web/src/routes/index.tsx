import { createRoute, redirect } from "@tanstack/react-router";

import { rootRoute } from "./root.tsx";

/** `/` has no page of its own; Apps is home. */
export const indexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/",
  beforeLoad: () => {
    throw redirect({ to: "/apps", replace: true });
  },
});
