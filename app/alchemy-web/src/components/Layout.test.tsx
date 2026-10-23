import { act, cleanup, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vite-plus/test";

import { FakeEventSource, renderAt } from "../test/render.tsx";

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
    vi.fn(async () => {
      if (!daemonUp) {
        throw new TypeError("connection refused");
      }
      return new Response(JSON.stringify({ version: "9.9.9" }), { status: 200 });
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
