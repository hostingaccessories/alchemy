import {
  type RouterHistory,
  createRoute,
  createRouter,
  lazyRouteComponent,
} from "@tanstack/react-router";

import { RouteError } from "./components/RouteError.tsx";
import { appsRoute } from "./routes/apps.tsx";
import { indexRoute } from "./routes/index.tsx";
import { machinesRoute } from "./routes/machines.tsx";
import { rootRoute } from "./routes/root.tsx";
import { settingsRoute } from "./routes/settings.tsx";

/**
 * Routes for development only: the component gallery. Built inside the
 * `import.meta.env.DEV` check, with a lazy component, so production bundles
 * leave out both the route and the gallery's code.
 */
function devRoutes() {
  if (!import.meta.env.DEV) {
    return [];
  }
  return [
    createRoute({
      getParentRoute: () => rootRoute,
      path: "/dev/components",
      component: lazyRouteComponent(
        () => import("./routes/dev-components.tsx"),
        "ComponentGallery",
      ),
    }),
  ];
}

/** Every route, built in code. Add a page by creating it in `routes/` and listing it here. */
export const routeTree = rootRoute.addChildren([
  indexRoute,
  appsRoute,
  machinesRoute,
  settingsRoute,
  ...devRoutes(),
]);

/**
 * Build the router. Browser history by default, in the desktop shell too:
 * both `alchemyd` and Tauri's asset protocol serve `index.html` for unknown
 * paths, so deep links resolve without hash history. Tests pass a memory
 * history. Unknown paths render the root 404, even under a real page such
 * as `/apps/missing`. A page that throws renders {@link RouteError} in its
 * place, inside the layout.
 */
export function createAppRouter(history?: RouterHistory) {
  return createRouter({
    routeTree,
    history,
    notFoundMode: "root",
    defaultErrorComponent: RouteError,
  });
}

declare module "@tanstack/react-router" {
  interface Register {
    router: ReturnType<typeof createAppRouter>;
  }
}
