import { act, cleanup, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vite-plus/test";

import type { App, AppList, MetricsResponse } from "../lib/api.ts";
import { FakeEventSource, renderAt } from "../test/render.tsx";

const blog: App = {
  name: "blog",
  status: "partial",
  dir: "/home/me/Alchemy/apps/blog",
  root: "/home/me/Alchemy/apps",
  compose_file: "compose.yaml",
  project: "blog",
  images: "registry",
  services: [
    {
      name: "db",
      containers: [
        {
          id: "1",
          name: "blog-db-1",
          image: "postgres:16",
          state: "running",
          health: "healthy",
        },
      ],
    },
    { name: "web", containers: [] },
  ],
  problems: [],
};

const broken: App = {
  ...blog,
  name: "wiki",
  status: "unknown",
  dir: "/opt/stacks/wiki",
  root: "/opt/stacks",
  project: null,
  images: null,
  services: [],
  problems: [
    { kind: "compose_error", message: "Compose can't read compose.yaml: yaml: line 4: oops" },
  ],
};

const home = {
  path: "/home/me/Alchemy/apps",
  home: true,
  state: "missing" as const,
  message: "This folder doesn't exist yet.",
  skipped: [],
};

const empty: AppList = { apps: [], unmanaged: [], dirs: [home] };

const full: AppList = {
  apps: [blog, broken],
  unmanaged: [{ project: "legacy", dir: "/home/me/legacy", status: "running", services: [] }],
  dirs: [
    { ...home, state: "ok", message: null },
    {
      path: "/opt/stacks",
      home: false,
      state: "ok",
      message: null,
      skipped: [
        {
          name: "My App",
          path: "/opt/stacks/My App",
          reason: "invalid_name",
          message: '"My App" can\'t be an app name. Rename the folder to use it.',
        },
      ],
    },
    {
      path: "/gone",
      home: false,
      state: "missing",
      message: "This folder doesn't exist.",
      skipped: [],
    },
  ],
};

const metrics: MetricsResponse = {
  at: 60,
  interval: 60,
  containers_in_vm: false,
  machine: null,
  projects: [
    { name: "blog", containers: 1, cpu: 12.5, memory: 2 * 1024 * 1024, net_rx: 0, net_tx: 0 },
  ],
  containers: [],
};

let apps: AppList = empty;

beforeEach(() => {
  apps = empty;
  FakeEventSource.instances = [];
  vi.stubGlobal("EventSource", FakeEventSource);
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string) => {
      if (url === "/api/apps") {
        return new Response(JSON.stringify(apps));
      }
      if (url === "/api/metrics") {
        return new Response(JSON.stringify(metrics));
      }
      return new Response(JSON.stringify({ version: "9.9.9", api: 1 }));
    }),
  );
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

async function sendApps(list: AppList) {
  await act(async () => {
    FakeEventSource.last().emit("apps", JSON.stringify(list));
    await new Promise((resolve) => setTimeout(resolve, 0));
  });
}

describe("Apps", () => {
  it("says where to put the first app and links to Settings", async () => {
    await renderAt("/apps");
    expect(await screen.findByText("No apps yet")).toBeTruthy();
    expect(
      screen.getByText(
        "Alchemy looks for apps in /home/me/Alchemy/apps. Each app is a folder there with a compose file in it.",
      ),
    ).toBeTruthy();
    const link = screen.getByRole("link", { name: "Choose where Alchemy looks for apps" });
    expect(link.getAttribute("href")).toBe("/settings");
    expect(screen.queryByText(/can't use/)).toBeNull();
  });

  it("lists apps with their status, problems, skipped folders, and unmanaged projects", async () => {
    apps = full;
    await renderAt("/apps");
    const list = await screen.findByRole("list", { name: "Your apps" });
    const rows = within(list).getAllByRole("link");
    expect(rows.map((row) => row.getAttribute("href"))).toEqual(["/apps/blog", "/apps/wiki"]);
    expect(within(rows[0]!).getByText("Partly running")).toBeTruthy();
    expect(within(rows[0]!).getByText("/home/me/Alchemy/apps/blog")).toBeTruthy();
    expect(within(rows[1]!).getByText("Status unknown")).toBeTruthy();
    cleanup();
    apps = {
      ...full,
      apps: [
        {
          ...blog,
          status: "stopped",
          stop_reason: { kind: "crash_looping", exit_code: 1, oom: false },
        },
      ],
    };
    await renderAt("/apps");
    expect(await screen.findByText("Stopped, keeps crashing (exit code 1)")).toBeTruthy();
    expect(
      within(rows[1]!).getByText("Compose can't read compose.yaml: yaml: line 4: oops"),
    ).toBeTruthy();

    const folders = screen.getByRole("region", { name: "Folders that need a look" });
    expect(within(folders).getByText("Alchemy can't use /gone")).toBeTruthy();
    expect(within(folders).getByText("1 folder wasn't added as an app")).toBeTruthy();
    expect(within(folders).getByText("/opt/stacks/My App")).toBeTruthy();

    const unmanaged = screen.getByRole("region", { name: "Running outside your app folders" });
    expect(within(unmanaged).getByText("legacy")).toBeTruthy();
    expect(within(unmanaged).getByText("/home/me/legacy")).toBeTruthy();
  });

  it("follows the apps event live", async () => {
    await renderAt("/apps");
    await screen.findByText("No apps yet");
    await sendApps({ ...empty, apps: [blog] });
    expect(await screen.findByText("Partly running")).toBeTruthy();
    await sendApps({ ...empty, apps: [{ ...blog, status: "running" }] });
    expect(await screen.findByText("Running")).toBeTruthy();
    expect(screen.queryByText("No apps yet")).toBeNull();
  });
});

describe("App", () => {
  it("shows an app's services, usage, and where it lives", async () => {
    apps = full;
    await renderAt("/apps/blog");
    expect(await screen.findByRole("heading", { name: "Services" })).toBeTruthy();
    const header = screen.getByRole("banner");
    expect(within(header).getByText("blog")).toBeTruthy();
    expect(within(header).getByText("Partly running")).toBeTruthy();
    const services = screen.getByRole("heading", { name: "Services" }).parentElement!;
    const rows = within(services).getAllByRole("listitem");
    expect(rows.map((row) => row.textContent)).toEqual(["dbRunning", "webNot running"]);
    expect(screen.getByText("/home/me/Alchemy/apps/blog")).toBeTruthy();
    expect(screen.getByText("compose.yaml")).toBeTruthy();
    expect(screen.getByText("Downloaded")).toBeTruthy();
    expect(await screen.findByText("12.5%")).toBeTruthy();
  });

  it("says why an app and its services aren't running", async () => {
    const oom = { kind: "crashed" as const, exit_code: 137, oom: true };
    const container = (
      id: string,
      state: "running" | "exited",
      stop_reason: App["stop_reason"],
    ) => ({
      id,
      name: `blog-${id}`,
      image: null,
      state,
      health: null,
      stop_reason,
    });
    apps = {
      ...full,
      apps: [
        {
          ...blog,
          stop_reason: oom,
          services: [
            { name: "db", containers: [container("db", "exited", oom)] },
            { name: "migrate", containers: [container("migrate", "exited", { kind: "finished" })] },
            { name: "web", containers: [container("web", "running", { kind: "running" })] },
          ],
        },
      ],
    };
    await renderAt("/apps/blog");
    const header = screen.getByRole("banner");
    expect(await within(header).findByText("Partly running, crashed (out of memory)")).toBeTruthy();
    const services = screen.getByRole("heading", { name: "Services" }).parentElement!;
    const rows = within(services).getAllByRole("listitem");
    expect(rows.map((row) => row.textContent)).toEqual([
      "dbCrashed (out of memory)",
      "migrateFinished",
      "webRunning",
    ]);
  });

  it("puts problems first", async () => {
    apps = full;
    await renderAt("/apps/wiki");
    expect(await screen.findByText("Compose can't read this app")).toBeTruthy();
    expect(screen.getByText("Compose can't read compose.yaml: yaml: line 4: oops")).toBeTruthy();
    expect(
      screen.getByText("Alchemy can't list this app's services until Compose can read it."),
    ).toBeTruthy();
    expect(screen.getByText("Not known yet")).toBeTruthy();
  });

  it("says when there's no such app", async () => {
    apps = full;
    await renderAt("/apps/nope");
    expect(await screen.findByText("There's no app named “nope”.")).toBeTruthy();
    expect(screen.getByRole("link", { name: "Go to Apps" })).toBeTruthy();
  });
});
