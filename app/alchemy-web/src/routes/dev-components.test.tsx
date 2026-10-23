import { cleanup, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vite-plus/test";

import { FakeEventSource, renderAt } from "../test/render.tsx";

beforeEach(() => {
  FakeEventSource.instances = [];
  vi.stubGlobal("EventSource", FakeEventSource);
  vi.stubGlobal(
    "fetch",
    vi.fn(async () => new Response(JSON.stringify({ version: "9.9.9", api: 1 }), { status: 200 })),
  );
  vi.spyOn(console, "error").mockImplementation(() => {});
  vi.spyOn(console, "warn").mockImplementation(() => {});
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

function heading() {
  return screen.getByRole("banner").querySelector("h1")?.textContent ?? "";
}

describe("component gallery", () => {
  it("renders every component in dev builds, outside the sidebar", async () => {
    await renderAt("/dev/components");
    expect(await screen.findByRole("region", { name: "Notice" })).toBeTruthy();
    expect(heading()).toBe("Components");
    for (const name of [
      "Buttons",
      "ProgressSteps",
      "Advanced",
      "ExternalLink",
      "ConfirmDialog",
      "EmptyState",
      "Loading",
      "ErrorState",
      "Route error boundary",
    ]) {
      expect(screen.getByRole("region", { name })).toBeTruthy();
    }
    const nav = screen.getByRole("navigation", { name: "Main" });
    expect(within(nav).queryByRole("link", { name: /components/i })).toBeNull();
  });
});

describe("route error boundary", () => {
  it("shows the error inside the layout and recovers on Try again", async () => {
    const user = userEvent.setup();
    await renderAt("/dev/components");
    await user.click(await screen.findByRole("button", { name: "Break this page" }));

    const alert = await screen.findByRole("alert");
    expect(within(alert).getByText("This page couldn't be shown")).toBeTruthy();
    expect(heading()).toBe("Something went wrong");
    expect(screen.getByRole("navigation", { name: "Main" })).toBeTruthy();
    expect(screen.queryByRole("region", { name: "Notice" })).toBeNull();

    await user.click(within(alert).getByRole("button", { name: "Details" }));
    expect(within(alert).getByText("Error: The gallery threw on purpose")).toBeTruthy();

    await user.click(within(alert).getByRole("button", { name: "Try again" }));
    expect(await screen.findByRole("region", { name: "Notice" })).toBeTruthy();
    expect(heading()).toBe("Components");
  });
});
