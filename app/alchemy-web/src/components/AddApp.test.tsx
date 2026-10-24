import { cleanup, fireEvent, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vite-plus/test";

import type { App, AppList, Operation } from "../lib/api.ts";
import { FakeEventSource, renderAt } from "../test/render.tsx";
import { STARTER_COMPOSE } from "./AddApp.tsx";

const home = {
  path: "/home/me/Alchemy/apps",
  home: true,
  state: "missing" as const,
  message: null,
  skipped: [],
};

const stacks = { ...home, path: "/opt/stacks", home: false, state: "ok" as const };

let apps: AppList = { apps: [], unmanaged: [], dirs: [home] };
let postReply: () => Response = () => new Response("{}");
const posts: unknown[] = [];

const shop: App = {
  name: "shop",
  status: "unknown",
  dir: "/opt/stacks/shop",
  root: "/opt/stacks",
  compose_file: "compose.yaml",
  project: "shop",
  images: "registry",
  services: [{ name: "web", containers: [] }],
  problems: [],
};

const upShop: Operation = {
  id: "op1",
  app: "shop",
  action: "up",
  state: "running",
  steps: [{ command: "up", label: "Starting the app", state: "active", message: null }],
  started_at: 1_700_000_000,
  finished_at: null,
  error: null,
};

beforeEach(() => {
  apps = { apps: [], unmanaged: [], dirs: [home] };
  posts.length = 0;
  FakeEventSource.instances = [];
  vi.stubGlobal("EventSource", FakeEventSource);
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string, init?: RequestInit) => {
      if (url === "/api/apps" && init?.method === "POST") {
        posts.push(JSON.parse(init.body as string));
        return postReply();
      }
      if (url === "/api/apps") {
        return new Response(JSON.stringify(apps));
      }
      if (url === "/api/operations") {
        return new Response(JSON.stringify({ operations: [] }));
      }
      if (url === "/api/metrics") {
        return new Response(JSON.stringify({ projects: [] }));
      }
      return new Response(JSON.stringify({ version: "9.9.9", api: 1 }));
    }),
  );
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

async function openForm() {
  await renderAt("/apps");
  fireEvent.click(await screen.findByRole("button", { name: "Add app" }));
  return screen.getByRole("form", { name: "Add an app" });
}

describe("AddApp", () => {
  it("creates an app in the chosen folder and opens it", async () => {
    apps = { ...apps, dirs: [home, stacks] };
    postReply = () => new Response(JSON.stringify(shop), { status: 201 });
    const { router } = await renderAt("/apps");
    fireEvent.click(await screen.findByRole("button", { name: "Add app" }));
    const form = screen.getByRole("form", { name: "Add an app" });
    expect(within(form).getByLabelText<HTMLTextAreaElement>("Compose file").value).toBe(
      STARTER_COMPOSE,
    );
    fireEvent.change(within(form).getByLabelText("Name"), { target: { value: " shop " } });
    fireEvent.change(within(form).getByLabelText("App folder"), {
      target: { value: "/opt/stacks" },
    });
    fireEvent.click(within(form).getByRole("button", { name: "Create app" }));

    await vi.waitFor(() => expect(router.state.location.pathname).toBe("/apps/shop"));
    expect(posts).toEqual([
      {
        name: "shop",
        folder: "/opt/stacks",
        compose: STARTER_COMPOSE,
        env: null,
        dry_run: false,
      },
    ]);
  });

  it("creates and starts an app, then shows its progress", async () => {
    postReply = () => new Response(JSON.stringify({ ...shop, operation: upShop }), { status: 201 });
    const { router } = await renderAt("/apps");
    fireEvent.click(await screen.findByRole("button", { name: "Add app" }));
    const form = screen.getByRole("form", { name: "Add an app" });
    fireEvent.change(within(form).getByLabelText("Name"), { target: { value: "shop" } });
    fireEvent.click(within(form).getByRole("button", { name: "Create and start" }));

    await vi.waitFor(() => expect(router.state.location.pathname).toBe("/apps/shop"));
    expect((await screen.findAllByText("Creating and starting shop…")).length).toBeGreaterThan(0);
    expect(posts).toEqual([
      {
        name: "shop",
        folder: null,
        compose: STARTER_COMPOSE,
        env: null,
        dry_run: false,
        start: true,
      },
    ]);
  });

  it("says when the app was created but not started", async () => {
    const why = "Docker isn't running, so Alchemy can't up shop";
    postReply = () =>
      new Response(JSON.stringify({ ...shop, start_error: { error: why, code: "conflict" } }), {
        status: 201,
      });
    const { router } = await renderAt("/apps");
    fireEvent.click(await screen.findByRole("button", { name: "Add app" }));
    const form = screen.getByRole("form", { name: "Add an app" });
    fireEvent.change(within(form).getByLabelText("Name"), { target: { value: "shop" } });
    fireEvent.click(within(form).getByRole("button", { name: "Create and start" }));

    const alert = await screen.findByRole("alert");
    expect(within(alert).getByText("Created shop, but couldn't start it")).toBeTruthy();
    expect(within(alert).getByText(why)).toBeTruthy();
    expect(within(form).getByRole("button", { name: "Create app" }).hasAttribute("disabled")).toBe(
      true,
    );
    expect(router.state.location.pathname).toBe("/apps");

    fireEvent.click(within(alert).getByRole("button", { name: "Open shop" }));
    await vi.waitFor(() => expect(router.state.location.pathname).toBe("/apps/shop"));
  });

  it("shows why the app wasn't created", async () => {
    postReply = () =>
      new Response(
        JSON.stringify({
          error: "services.web.ports must be a list",
          code: "invalid",
        }),
        { status: 422 },
      );
    const form = await openForm();
    expect(within(form).queryByLabelText("App folder")).toBeNull();
    fireEvent.change(within(form).getByLabelText("Name"), { target: { value: "blog" } });
    fireEvent.click(within(form).getByRole("button", { name: "Create app" }));
    const alert = await screen.findByRole("alert");
    expect(within(alert).getByText("Alchemy couldn't create this app")).toBeTruthy();
    expect(within(alert).getByText("services.web.ports must be a list")).toBeTruthy();
    expect(posts).toEqual([
      { name: "blog", folder: null, compose: STARTER_COMPOSE, env: null, dry_run: false },
    ]);

    fireEvent.click(within(form).getByRole("button", { name: "Cancel" }));
    expect(screen.queryByRole("form", { name: "Add an app" })).toBeNull();
  });
});
