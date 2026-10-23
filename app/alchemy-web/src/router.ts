import { type RouterHistory, createRouter } from "@tanstack/react-router";

import { appsRoute } from "./routes/apps.tsx";
import { indexRoute } from "./routes/index.tsx";
import { machinesRoute } from "./routes/machines.tsx";
import { rootRoute } from "./routes/root.tsx";
import { settingsRoute } from "./routes/settings.tsx";

/** Every route, built in code. Add a page by creating it in `routes/` and listing it here. */
export const routeTree = rootRoute.addChildren([
  indexRoute,
  appsRoute,
  machinesRoute,
  settingsRoute,
]);

/**
 * Build the router. Browser history by default, in the desktop shell too:
 * both `alchemyd` and Tauri's asset protocol serve `index.html` for unknown
 * paths, so deep links resolve without hash history. Tests pass a memory
 * history. Unknown paths render the root 404, even under a real page such
 * as `/apps/missing`.
 */
export function createAppRouter(history?: RouterHistory) {
  return createRouter({ routeTree, history, notFoundMode: "root" });
}

declare module "@tanstack/react-router" {
  interface Register {
    router: ReturnType<typeof createAppRouter>;
  }
}
