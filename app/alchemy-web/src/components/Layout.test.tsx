import { act, cleanup, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vite-plus/test";

import { FakeEventSource, renderAt } from "../test/render.tsx";

vi.mock("@tauri-apps/api/core", () => ({
  invoke: async () => {
    throw new Error("no token file");
  },
}));
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({ setTheme: async () => {} }),
}));

/** Whether the stubbed `GET /api/version` answers. */
let daemonUp = true;

function dispatch(type: "open" | "error") {
  act(() => {
    FakeEventSource.last().dispatchEvent(new Event(type));
  });
}

function banner() {
  return screen.queryByRole("alert");
}

beforeEach(() => {
  daemonUp = true;
  FakeEventSource.instances = [];
  vi.stubGlobal("EventSource", FakeEventSource);
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string) => {
      if (!daemonUp) {
        throw new TypeError("connection refused");
      }
      const body =
        url === "/api/apps" ? { apps: [], unmanaged: [], dirs: [] } : { version: "9.9.9", api: 1 };
      return new Response(JSON.stringify(body), { status: 200 });
    }),
  );
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("daemon-unreachable banner", () => {
  it("stays hidden while alchemyd answers", async () => {
    await renderAt("/apps");
    await screen.findByText("alchemyd 9.9.9");
    expect(banner()).toBeNull();
  });

  it("leads with the problem and folds the detail", async () => {
    daemonUp = false;
    await renderAt("/apps");
    const alert = await screen.findByRole("alert");
    expect(within(alert).getByText("Alchemy can't reach alchemyd")).toBeTruthy();
    expect(
      within(alert).getByText(
        "Check that alchemyd is running on this machine. Alchemy reconnects on its own.",
      ),
    ).toBeTruthy();
    expect(within(alert).queryByText("alchemyd is not reachable")).toBeNull();
    await userEvent.setup().click(within(alert).getByRole("button", { name: "Details" }));
    expect(within(alert).getByText("alchemyd is not reachable")).toBeTruthy();
  });

  it("shows on every page, the page still renders beneath it", async () => {
    daemonUp = false;
    await renderAt("/settings");
    expect(await screen.findByRole("alert")).toBeTruthy();
    expect(screen.getByRole("heading", { name: "Appearance" })).toBeTruthy();
  });

  it("toggles with the daemon without a reload", async () => {
    await renderAt("/apps");
    await screen.findByText("alchemyd 9.9.9");

    daemonUp = false;
    dispatch("error");
    expect(await screen.findByRole("alert")).toBeTruthy();

    daemonUp = true;
    dispatch("open");
    await screen.findByText("alchemyd 9.9.9");
    expect(banner()).toBeNull();

    daemonUp = false;
    dispatch("error");
    expect(await screen.findByRole("alert")).toBeTruthy();
    expect(FakeEventSource.instances).toHaveLength(1);
  });
});

describe("signed-out banner", () => {
  /** Whether the stubbed daemon lets this page in. */
  let signedIn = false;

  beforeEach(() => {
    signedIn = false;
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string) => {
        if (url === "/api/version") {
          return new Response(JSON.stringify({ version: "9.9.9", api: 1 }));
        }
        if (!signedIn) {
          return new Response(JSON.stringify({ error: "not signed in", code: "unauthorized" }), {
            status: 401,
          });
        }
        if (url === "/api/session") {
          return new Response(JSON.stringify({ auth: "cookie" }));
        }
        return new Response(JSON.stringify({ error: "not found", code: "not_found" }), {
          status: 404,
        });
      }),
    );
  });

  afterEach(() => {
    delete (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__;
  });

  it("tells a browser to run alchemy open, while alchemyd still shows as running", async () => {
    await renderAt("/settings");
    const alert = await screen.findByRole("alert");
    expect(within(alert).getByText("You're signed out of alchemyd")).toBeTruthy();
    expect(within(alert).getByText("alchemy open")).toBeTruthy();
    expect(await screen.findByText("alchemyd 9.9.9")).toBeTruthy();
    expect(screen.queryByText("Alchemy can't reach alchemyd")).toBeNull();
  });

  it("clears once the stream connects signed in", async () => {
    await renderAt("/settings");
    await screen.findByText("You're signed out of alchemyd");
    dispatch("error");
    signedIn = true;
    dispatch("open");
    await waitFor(() => expect(screen.queryByText("You're signed out of alchemyd")).toBeNull());
  });

  it("stays hidden while signed in", async () => {
    signedIn = true;
    await renderAt("/settings");
    await screen.findByText("alchemyd 9.9.9");
    expect(banner()).toBeNull();
  });

  it("tells the desktop shell its token didn't work", async () => {
    (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
    await renderAt("/settings");
    const alert = await screen.findByRole("alert");
    expect(within(alert).getByText("Alchemy can't sign in to alchemyd")).toBeTruthy();
    expect(within(alert).queryByText("alchemy open")).toBeNull();
  });
});
