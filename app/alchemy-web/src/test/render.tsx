import { QueryClient } from "@tanstack/react-query";
import { createMemoryHistory } from "@tanstack/react-router";
import { render, screen } from "@testing-library/react";
import { vi } from "vite-plus/test";

import { App } from "../App.tsx";
import { createAppRouter } from "../router.ts";

export { FakeEventSource } from "./event-source.ts";

/**
 * Render the whole app at `path` with in-memory history, and wait for the
 * layout to mount. Callers stub `fetch` and `EventSource` first and unstub
 * globals afterwards. Stubs `scrollTo`, which jsdom lacks and the router's
 * scroll handling calls.
 */
export async function renderAt(path: string) {
  vi.stubGlobal("scrollTo", vi.fn());
  const router = createAppRouter(createMemoryHistory({ initialEntries: [path] }));
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const result = render(<App router={router} queryClient={queryClient} />);
  await screen.findByRole("navigation", { name: "Main" });
  return { ...result, router };
}
