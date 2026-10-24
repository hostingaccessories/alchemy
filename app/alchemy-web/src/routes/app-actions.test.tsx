import { act, cleanup, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vite-plus/test";

import type { App, AppList, Operation, OperationList } from "../lib/api.ts";
import { FakeEventSource, renderAt } from "../test/render.tsx";

const blog: App = {
  name: "blog",
  status: "running",
  stop_reason: { kind: "running" },
  dir: "/home/me/Alchemy/apps/blog",
  root: "/home/me/Alchemy/apps",
  compose_file: "compose.yaml",
  project: "blog",
  images: "registry",
  services: [],
  problems: [],
};

const apps: AppList = { apps: [blog], unmanaged: [], dirs: [] };

function update(overrides: Partial<Operation> = {}): Operation {
  return {
    id: "op1",
    app: "blog",
    action: "update",
    state: "running",
    steps: [
      {
        command: "pull",
        label: "Downloading updates",
        state: "active",
        message: "1 of 2 images downloaded",
      },
      { command: "up", label: "Starting the new version", state: "pending", message: null },
    ],
    started_at: 100,
    finished_at: null,
    error: null,
    ...overrides,
  };
}

let operations: OperationList = { operations: [] };
let actionReply: { status: number; body: unknown } = {
  status: 202,
  body: { operation: update() },
};
const calls: { url: string; method: string; body: unknown }[] = [];

beforeEach(() => {
  operations = { operations: [] };
  actionReply = { status: 202, body: { operation: update() } };
  calls.length = 0;
  FakeEventSource.instances = [];
  vi.stubGlobal("EventSource", FakeEventSource);
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string, init?: RequestInit) => {
      const method = init?.method ?? "GET";
      calls.push({
        url,
        method,
        body: typeof init?.body === "string" ? JSON.parse(init.body) : undefined,
      });
      if (url === "/api/apps") return new Response(JSON.stringify(apps));
      if (url === "/api/operations") return new Response(JSON.stringify(operations));
      if (url === "/api/metrics") return new Response(JSON.stringify({ projects: [] }));
      if (url === "/api/apps/blog/actions") {
        return new Response(JSON.stringify(actionReply.body), { status: actionReply.status });
      }
      if (url === "/api/operations/op1/cancel") {
        return new Response(JSON.stringify({ operation: update() }), { status: 202 });
      }
      return new Response(JSON.stringify({ version: "9.9.9", api: 1 }));
    }),
  );
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

function stream(path: string): FakeEventSource {
  const source = FakeEventSource.instances.findLast((s) => s.url.endsWith(path));
  if (!source) throw new Error(`no stream for ${path}`);
  return source;
}

async function emit(source: FakeEventSource, event: string, data: unknown) {
  await act(async () => {
    source.emit(event, JSON.stringify(data));
    await new Promise((resolve) => setTimeout(resolve, 0));
  });
}

function posts() {
  return calls.filter((call) => call.method === "POST");
}

describe("App actions", () => {
  it("starts an action and shows its steps and output as they arrive", async () => {
    const user = userEvent.setup();
    await renderAt("/apps/blog");
    const actions = (await screen.findByRole("heading", { name: "Actions" })).parentElement!;
    await user.click(within(actions).getByRole("button", { name: "Update" }));
    expect(posts()).toEqual([
      { url: "/api/apps/blog/actions", method: "POST", body: { action: "update" } },
    ]);

    const steps = await screen.findByRole("list", { name: "Updating blog…" });
    expect(within(steps).getByText("Downloading updates…")).toBeTruthy();
    expect(within(steps).getByText("1 of 2 images downloaded")).toBeTruthy();
    expect(within(actions).getByRole("button", { name: "Start" })).toHaveProperty("disabled", true);

    await emit(stream("/api/operations/op1/events"), "output", {
      seq: 0,
      step: 0,
      text: "Image nginx Pulled",
    });
    await user.click(within(steps).getByRole("button", { name: "Show output" }));
    expect(within(steps).getByText("Image nginx Pulled")).toBeTruthy();

    const done = update({
      state: "succeeded",
      finished_at: 130,
      steps: update().steps.map((step) => ({ ...step, state: "done" as const })),
    });
    await emit(stream("/api/events"), "operations", { operations: [done] });
    expect(await screen.findByText("Updated blog")).toBeTruthy();
    expect(within(actions).getByRole("button", { name: "Start" })).toHaveProperty(
      "disabled",
      false,
    );
  });

  it("shows an operation another client started, and cancels it", async () => {
    const user = userEvent.setup();
    operations = { operations: [update({ action: "stop" })] };
    await renderAt("/apps/blog");
    expect(await screen.findByText("Stopping blog…")).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "Cancel" }));
    expect(posts()).toEqual([
      { url: "/api/operations/op1/cancel", method: "POST", body: undefined },
    ]);

    await emit(stream("/api/events"), "operations", {
      operations: [
        update({
          action: "stop",
          state: "cancelled",
          error: "Cancelled before it finished",
          steps: [
            {
              command: "stop",
              label: "Stopping the app",
              state: "failed",
              message: "Cancelled",
            },
          ],
        }),
      ],
    });
    expect(await screen.findByText("Stop was cancelled")).toBeTruthy();
    expect(screen.getByText("Cancelled before it finished")).toBeTruthy();
  });

  it("asks before taking an app down", async () => {
    const user = userEvent.setup();
    actionReply = { status: 202, body: { operation: update({ action: "down" }) } };
    await renderAt("/apps/blog");
    await user.click(await screen.findByRole("button", { name: "More actions" }));
    await user.click(screen.getByRole("button", { name: "Take down" }));
    const dialog = screen.getByRole("alertdialog", { name: "Take blog down?" });
    await user.click(within(dialog).getByRole("button", { name: "Cancel" }));
    expect(posts()).toEqual([]);

    await user.click(screen.getByRole("button", { name: "Take down" }));
    const again = screen.getByRole("alertdialog", { name: "Take blog down?" });
    await user.click(within(again).getByRole("button", { name: "Take down" }));
    expect(posts()).toEqual([
      { url: "/api/apps/blog/actions", method: "POST", body: { action: "down" } },
    ]);
    expect(await screen.findByText("Taking down blog…")).toBeTruthy();
  });

  it("says why alchemyd refused an action", async () => {
    const user = userEvent.setup();
    actionReply = {
      status: 409,
      body: {
        error: "blog is busy with another operation (op0); wait for it to finish",
        code: "conflict",
      },
    };
    await renderAt("/apps/blog");
    await user.click(await screen.findByRole("button", { name: "Restart" }));
    const alert = await screen.findByRole("alert");
    expect(within(alert).getByText(/blog is busy/)).toBeTruthy();
  });

  it("waits for Compose to read the app", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string) => {
        if (url === "/api/apps") {
          return new Response(
            JSON.stringify({ ...apps, apps: [{ ...blog, project: null, images: null }] }),
          );
        }
        if (url === "/api/operations") return new Response(JSON.stringify({ operations: [] }));
        return new Response(JSON.stringify({ version: "9.9.9", api: 1 }));
      }),
    );
    await renderAt("/apps/blog");
    expect(
      await screen.findByText("Alchemy can run this app once Compose can read its files."),
    ).toBeTruthy();
    expect(screen.getByRole("button", { name: "Start" })).toHaveProperty("disabled", true);
  });
});
