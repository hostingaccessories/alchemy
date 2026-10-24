import { cleanup, fireEvent, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vite-plus/test";

import type { AppSettings } from "../lib/api.ts";
import { FakeEventSource, renderAt } from "../test/render.tsx";

const DEFAULT = "/home/me/Alchemy/apps";

const settings: AppSettings = { home: DEFAULT, default_home: DEFAULT, extra: ["/opt/stacks"] };

/** Every `PUT /api/settings/apps` body, parsed. */
let puts: unknown[] = [];
/** What the next PUT answers: the saved settings, or a refusal. */
let putAnswer: () => Response = () => new Response("{}");

beforeEach(() => {
  puts = [];
  FakeEventSource.instances = [];
  vi.stubGlobal("EventSource", FakeEventSource);
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string, init?: RequestInit) => {
      if (url === "/api/settings/apps" && init?.method === "PUT") {
        puts.push(JSON.parse(init.body as string));
        return putAnswer();
      }
      if (url === "/api/settings/apps") {
        return new Response(JSON.stringify(settings));
      }
      return new Response(JSON.stringify({ version: "9.9.9", api: 1 }));
    }),
  );
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

function input(name: string) {
  return screen.getByRole<HTMLInputElement>("textbox", { name });
}

describe("app folders setting", () => {
  it("shows the folders in effect", async () => {
    await renderAt("/settings");
    expect(
      (await screen.findByRole<HTMLInputElement>("textbox", { name: "Home folder" })).value,
    ).toBe(DEFAULT);
    expect(input("Other app folder 1").value).toBe("/opt/stacks");
    expect(screen.queryByRole("button", { name: "Use the default" })).toBeNull();
  });

  it("saves a new home and folders, keeping the default as null", async () => {
    putAnswer = () =>
      new Response(JSON.stringify({ ...settings, home: "/srv/apps", extra: ["/data/apps"] }));
    await renderAt("/settings");
    const home = await screen.findByRole<HTMLInputElement>("textbox", { name: "Home folder" });

    fireEvent.change(home, { target: { value: "/srv/apps" } });
    fireEvent.click(screen.getByRole("button", { name: "Remove /opt/stacks" }));
    fireEvent.click(screen.getByRole("button", { name: "Add a folder" }));
    fireEvent.change(input("Other app folder 1"), { target: { value: " /data/apps " } });
    fireEvent.click(screen.getByRole("button", { name: "Add a folder" }));
    fireEvent.click(screen.getByRole("button", { name: "Save" }));

    expect(await screen.findByText("Saved. Alchemy is looking in these folders now.")).toBeTruthy();
    expect(puts).toEqual([{ home: "/srv/apps", extra: ["/data/apps"] }]);

    fireEvent.click(screen.getByRole("button", { name: "Use the default" }));
    expect(home.value).toBe(DEFAULT);
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    await vi.waitFor(() => expect(puts).toHaveLength(2));
    expect(puts[1]).toEqual({ home: null, extra: ["/data/apps"] });
  });

  it("shows why alchemyd refused the folders", async () => {
    putAnswer = () =>
      new Response(
        JSON.stringify({
          error: "apps is not a full path; use one that starts with /",
          code: "invalid",
        }),
        { status: 422 },
      );
    await renderAt("/settings");
    fireEvent.change(await screen.findByRole("textbox", { name: "Home folder" }), {
      target: { value: "apps" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(await screen.findByText("Couldn't save your app folders")).toBeTruthy();
    expect(screen.getByText("apps is not a full path; use one that starts with /")).toBeTruthy();
  });
});
