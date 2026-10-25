import { cleanup, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vite-plus/test";

import type { ShellLaunch } from "../lib/shell.ts";
import { FakeEventSource, renderAt } from "../test/render.tsx";

const tauri = vi.hoisted(() => ({
  launch: null as ShellLaunch | null,
  invoke: vi.fn(async (command: string): Promise<unknown> => {
    if (command === "launch_state" && tauri.launch !== null) {
      return tauri.launch;
    }
    if (command === "start_daemon") {
      return null;
    }
    throw new Error(`no ${command}`);
  }),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: tauri.invoke }));
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({ setTheme: async () => {} }),
}));

beforeEach(() => {
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
  tauri.launch = null;
  tauri.invoke.mockClear();
  FakeEventSource.instances = [];
  vi.stubGlobal("EventSource", FakeEventSource);
  vi.stubGlobal(
    "fetch",
    vi.fn(async () => {
      throw new TypeError("connection refused");
    }),
  );
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  delete (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__;
});

describe("the shell's daemon banner", () => {
  it("says Alchemy is starting while the shell starts alchemyd", async () => {
    tauri.launch = { kind: "starting" };
    await renderAt("/apps");
    const status = await screen.findByText("Starting Alchemy…");
    expect(status).toBeTruthy();
    expect(screen.queryByText("Alchemy can't reach alchemyd")).toBeNull();
  });

  it("shows why starting failed and starts it again on request", async () => {
    tauri.launch = { kind: "failed", message: "alchemyd didn't answer within 30 seconds." };
    await renderAt("/apps");
    const title = await screen.findByText("Alchemy couldn't start alchemyd");
    const alert = title.closest("[role=alert]") as HTMLElement;
    expect(within(alert).getByText("alchemyd didn't answer within 30 seconds.")).toBeTruthy();
    await userEvent.setup().click(within(alert).getByRole("button", { name: "Try again" }));
    expect(tauri.invoke).toHaveBeenCalledWith("start_daemon");
  });

  it("offers to start alchemyd after it stopped", async () => {
    tauri.launch = { kind: "stopped" };
    await renderAt("/apps");
    expect(await screen.findByRole("button", { name: "Start Alchemy" })).toBeTruthy();
  });

  it("says why it can't start alchemyd, with no button", async () => {
    tauri.launch = { kind: "unavailable", message: "Install the .deb package." };
    await renderAt("/apps");
    const title = await screen.findByText("Alchemy can't start alchemyd from here");
    const alert = title.closest("[role=alert]") as HTMLElement;
    expect(within(alert).getByText("Install the .deb package.")).toBeTruthy();
    expect(within(alert).queryByRole("button")).toBeNull();
  });

  it("falls back to the usual banner while reconnecting", async () => {
    tauri.launch = { kind: "lost" };
    await renderAt("/apps");
    expect(await screen.findByText("Alchemy can't reach alchemyd")).toBeTruthy();
  });
});
