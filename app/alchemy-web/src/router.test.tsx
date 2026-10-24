import { act, cleanup, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vite-plus/test";

import { FakeEventSource, renderAt } from "./test/render.tsx";

/** What the stubbed daemon answers, by URL; anything else gets the version. */
const ANSWERS: Record<string, unknown> = {
  "/api/apps": { apps: [], unmanaged: [], dirs: [] },
  "/api/ports": { ports: [], warnings: [] },
  "/api/network": {
    engine_ready: true,
    lan: [],
    routes: [],
    bridge: null,
    pools: [],
    pools_source: "defaults",
    networks: [],
    alchemy: { name: "alchemy", state: "ready", subnet: "172.18.0.0/24" },
    warnings: [],
    proxy: {
      reachability: "published_only",
      host: { port: 80, app: null, state: "off", message: null, fix: null },
      ports: [],
    },
  },
  "/api/machine/privileges": { privileges: [] },
  "/api/settings/apps": {
    home: "/home/me/Alchemy/apps",
    default_home: "/home/me/Alchemy/apps",
    extra: [],
  },
};

beforeEach(() => {
  FakeEventSource.instances = [];
  vi.stubGlobal("EventSource", FakeEventSource);
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string) => {
      if (url.startsWith("/api/metrics")) {
        return new Promise<Response>(() => {});
      }
      const body = ANSWERS[url] ?? { version: "9.9.9", api: 1 };
      return new Response(JSON.stringify(body), { status: 200 });
    }),
  );
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

function heading() {
  return screen.getByRole("banner").querySelector("h1")?.textContent ?? "";
}

function current() {
  return within(screen.getByRole("navigation", { name: "Main" }))
    .getAllByRole("link")
    .filter((link) => link.getAttribute("aria-current") === "page")
    .map((link) => link.textContent);
}

describe("router", () => {
  const pages: [string, string, string][] = [
    ["/apps", "Apps", "No apps yet"],
    ["/machines", "Machines", "This machine"],
    ["/settings", "Settings", "Appearance"],
  ];

  it.each(pages)("renders %s in the layout", async (path, title, text) => {
    await renderAt(path);
    expect(heading()).toBe(title);
    expect(current()).toEqual([title]);
    expect(await within(screen.getByRole("main")).findByText(text)).toBeTruthy();
    expect(await screen.findByText("alchemyd 9.9.9")).toBeTruthy();
  });

  it("redirects / to Apps", async () => {
    const { router } = await renderAt("/");
    expect(await screen.findByText("No apps yet")).toBeTruthy();
    expect(router.state.location.pathname).toBe("/apps");
  });

  const missing: [string, string[]][] = [
    ["/nope", []],
    ["/stacks", []],
    ["/apps/blog/missing", ["Apps"]],
  ];

  it.each(missing)("renders the 404 inside the layout for %s", async (path, active) => {
    await renderAt(path);
    expect(await screen.findByText("There's nothing at this address.")).toBeTruthy();
    expect(heading()).toBe("Not found");
    expect(current()).toEqual(active);
    expect(screen.getByRole("link", { name: "Go to Apps" })).toBeTruthy();
  });

  it("navigates from the sidebar", async () => {
    const { router } = await renderAt("/apps");
    const nav = within(screen.getByRole("navigation", { name: "Main" }));
    act(() => {
      nav.getByRole("link", { name: "Machines" }).click();
    });
    expect(await screen.findByText("This machine")).toBeTruthy();
    expect(router.state.location.pathname).toBe("/machines");
    expect(heading()).toBe("Machines");
  });

  it("keeps one event stream across navigation", async () => {
    const { router } = await renderAt("/apps");
    await act(() => router.navigate({ to: "/machines" }));
    await act(() => router.navigate({ to: "/settings" }));
    expect(FakeEventSource.instances).toHaveLength(1);
  });

  it("uses the personas' words in the sidebar", async () => {
    await renderAt("/apps");
    const nav = screen.getByRole("navigation", { name: "Main" });
    expect(nav.textContent).toBe("AppsMachinesSettings");
    expect(document.body.textContent).not.toMatch(/\b(stack|container|node)s?\b/i);
  });
});
