import { act, cleanup, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vite-plus/test";

import type {
  App,
  AppList,
  EngineStatus,
  MetricsHistory,
  MetricsResponse,
  PortList,
} from "../lib/api.ts";
import { FakeEventSource, renderAt } from "../test/render.tsx";

const ready: EngineStatus = {
  state: "ready",
  flavor: "docker_desktop",
  engine_version: "29.8.1",
  api_version: "1.52",
  compose_version: "2.40.0",
  desktop_version: "4.93.0",
  endpoint: "unix:///Users/me/.docker/run/docker.sock",
};

const unknown: EngineStatus = {
  state: "not_running",
  flavor: null,
  engine_version: null,
  api_version: null,
  compose_version: null,
  desktop_version: null,
  endpoint: null,
};

/** Whether the stubbed `GET /api/version` answers. */
let daemonUp = true;

/** Every `/api/metrics/history` URL requested, in order. */
let historyRequests: string[] = [];

/** Whether the stubbed daemon predates `/api/metrics`. */
let oldDaemon = false;

/** What the stubbed `GET /api/ports` answers; `null` never answers. */
let portList: PortList | null = null;

const GIB = 1024 ** 3;

const sample: MetricsResponse = {
  at: 7200,
  interval: 60,
  containers_in_vm: false,
  machine: {
    cpu: 12.5,
    cpu_max: null,
    cpus: 8,
    load: [0.5, 0.25, 0.75],
    memory_used: 4 * GIB,
    memory_total: 16 * GIB,
    swap_used: 0,
    swap_total: 2 * GIB,
    disk_used: 100 * GIB,
    disk_total: 500 * GIB,
    disk_read: 2048,
    disk_write: null,
    net_rx: 1536,
    net_tx: 512,
  },
  projects: [
    { name: "blog", containers: 2, cpu: 3.5, memory: 300 * 1024 ** 2, net_rx: 100, net_tx: null },
  ],
  containers: [],
};

function history(range: MetricsHistory["range"]): MetricsHistory {
  const point = (t: number) => ({ ...sample, t, containers: [] });
  return { range, step: 1200, project: null, points: [point(4800), point(6000)] };
}

const source = () => FakeEventSource.last();

/** Send an `engine` event and wait for Query's batched notification to render. */
async function sendEngine(status: EngineStatus) {
  await act(async () => {
    source().emit("engine", JSON.stringify(status));
    await new Promise((resolve) => setTimeout(resolve, 0));
  });
}

function dispatch(type: "open" | "error") {
  act(() => {
    source().dispatchEvent(new Event(type));
  });
}

function renderMachines() {
  return renderAt("/machines");
}

async function renderOnline() {
  await renderMachines();
  await screen.findByText("alchemyd 9.9.9");
}

function docker() {
  return screen.getByRole("status", { name: "Docker" });
}

/** Send a `metrics` event and wait for it to render. */
async function sendMetrics(metrics: MetricsResponse) {
  await act(async () => {
    source().emit("metrics", JSON.stringify(metrics));
    await new Promise((resolve) => setTimeout(resolve, 0));
  });
}

beforeEach(() => {
  daemonUp = true;
  historyRequests = [];
  oldDaemon = false;
  portList = null;
  FakeEventSource.instances = [];
  vi.stubGlobal("EventSource", FakeEventSource);
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string) => {
      if (!daemonUp) {
        throw new TypeError("connection refused");
      }
      if (oldDaemon && (url.startsWith("/api/metrics") || url === "/api/ports")) {
        return new Response(JSON.stringify({ error: "not found", code: "not_found" }), {
          status: 404,
        });
      }
      if (url === "/api/ports") {
        return portList === null
          ? new Promise<Response>(() => {})
          : new Response(JSON.stringify(portList), { status: 200 });
      }
      if (url === "/api/engine" || url === "/api/metrics" || url === "/api/apps") {
        return new Promise<Response>(() => {});
      }
      if (url.startsWith("/api/metrics/history?")) {
        historyRequests.push(url);
        const range = new URL(url, "http://x").searchParams.get("range") as MetricsHistory["range"];
        return new Response(JSON.stringify(history(range)), { status: 200 });
      }
      return new Response(JSON.stringify({ version: "9.9.9" }), { status: 200 });
    }),
  );
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("Machines", () => {
  it("shows the daemon version when alchemyd answers", async () => {
    await renderMachines();
    expect(await screen.findByText("alchemyd 9.9.9")).toBeTruthy();
  });

  it("reports the daemon as unreachable when the request fails", async () => {
    daemonUp = false;
    await renderMachines();
    expect(await screen.findByText("alchemyd is not reachable")).toBeTruthy();
    expect(screen.queryByRole("status", { name: "Docker" })).toBeNull();
    expect(screen.getByText("Docker status appears once alchemyd is reachable.")).toBeTruthy();
  });

  it("checks Docker until the first engine event arrives", async () => {
    await renderOnline();
    expect(source().url).toBe("/api/events");
    expect(within(docker()).getByText("Checking Docker…")).toBeTruthy();
    expect(within(docker()).getByRole("img", { name: "In progress" })).toBeTruthy();
  });

  const states: [string, EngineStatus, string[]][] = [
    [
      "not installed",
      { ...unknown, state: "not_installed" },
      ["Docker isn't installed", "docs.docker.com/get-started/get-docker"],
    ],
    [
      "Docker Desktop not running",
      { ...unknown, flavor: "docker_desktop" },
      ["Docker Desktop isn't running. Open it to continue."],
    ],
    [
      "Docker Engine not running",
      { ...unknown, flavor: "docker_engine", endpoint: "unix:///var/run/docker.sock" },
      [
        "Docker Engine isn't running. Start it to continue.",
        "sudo systemctl start docker",
        "Looking for Docker at unix:///var/run/docker.sock",
      ],
    ],
    [
      "unknown flavor not running",
      unknown,
      ["Docker isn't running. Open Docker Desktop, or start the Docker service, to continue."],
    ],
    [
      "Docker Desktop starting",
      { ...unknown, state: "starting", flavor: "docker_desktop" },
      ["Docker Desktop is starting…"],
    ],
    ["starting", { ...unknown, state: "starting" }, ["Docker is starting…"]],
    ["ready", ready, ["Docker is ready", "Docker Desktop 4.93.0 · Engine 29.8.1 · Compose 2.40.0"]],
    [
      "ready with a v-prefixed Compose version",
      { ...ready, flavor: "docker_engine", desktop_version: null, compose_version: "v2.29.1" },
      ["Docker is ready", "Engine 29.8.1 · Compose 2.29.1"],
    ],
    [
      "ready without Compose",
      { ...ready, compose_version: null },
      ["Docker Compose wasn't found", "Docker Desktop 4.93.0 · Engine 29.8.1"],
    ],
    [
      "unsupported",
      { ...unknown, state: "unsupported", endpoint: "tcp://10.0.0.5:2375" },
      ["Alchemy can't use this Docker connection", "tcp://10.0.0.5:2375", "unix://"],
    ],
    [
      "a state this build doesn't know",
      { ...unknown, state: "paused" as EngineStatus["state"] },
      ["Docker is in a state this version of Alchemy doesn't recognize. Update Alchemy."],
    ],
  ];

  it.each(states)("shows Docker %s", async (_name, status, texts) => {
    await renderOnline();
    await sendEngine(status);
    for (const text of texts) {
      expect(within(docker()).getByText(text)).toBeTruthy();
    }
  });

  it("shows Starting with a labelled spinner", async () => {
    await renderOnline();
    await sendEngine({ ...unknown, state: "starting" });
    expect(within(docker()).getByRole("img", { name: "In progress" })).toBeTruthy();
  });

  it("follows engine changes live", async () => {
    await renderOnline();
    await sendEngine(ready);
    expect(within(docker()).getByText("Docker is ready")).toBeTruthy();

    await sendEngine({ ...unknown, flavor: "docker_desktop" });
    expect(
      within(docker()).getByText("Docker Desktop isn't running. Open it to continue."),
    ).toBeTruthy();
    expect(within(docker()).queryByText("Docker is ready")).toBeNull();

    await sendEngine({ ...unknown, state: "starting", flavor: "docker_desktop" });
    expect(within(docker()).getByText("Docker Desktop is starting…")).toBeTruthy();

    await sendEngine(ready);
    expect(within(docker()).getByText("Docker is ready")).toBeTruthy();
    expect(FakeEventSource.instances).toHaveLength(1);
  });

  it("falls back to offline when the stream drops and the daemon is gone, then recovers", async () => {
    await renderOnline();
    await sendEngine(ready);

    daemonUp = false;
    dispatch("error");
    expect(await screen.findByText("alchemyd is not reachable")).toBeTruthy();
    expect(screen.queryByRole("status", { name: "Docker" })).toBeNull();

    daemonUp = true;
    dispatch("open");
    expect(await screen.findByText("alchemyd 9.9.9")).toBeTruthy();
    await sendEngine({ ...unknown, flavor: "docker_desktop" });
    expect(
      within(docker()).getByText("Docker Desktop isn't running. Open it to continue."),
    ).toBeTruthy();
  });

  it("keeps the last engine status while the stream reconnects to a reachable daemon", async () => {
    await renderOnline();
    await sendEngine(ready);
    dispatch("error");
    await act(async () => {});
    expect(screen.getByText("alchemyd 9.9.9")).toBeTruthy();
    expect(within(docker()).getByText("Docker is ready")).toBeTruthy();
    dispatch("open");
    await sendEngine({ ...unknown, state: "starting" });
    expect(within(docker()).getByText("Docker is starting…")).toBeTruthy();
  });

  it("closes the stream on unmount", async () => {
    const { unmount } = await renderMachines();
    await screen.findByText("alchemyd 9.9.9");
    const opened = source();
    expect(opened.closed).toBe(false);
    unmount();
    expect(opened.closed).toBe(true);
  });

  it("waits for the first metrics sample", async () => {
    await renderOnline();
    expect(screen.getByText("Measuring this machine…")).toBeTruthy();
    await sendMetrics({ ...sample, at: null, machine: null, projects: [] });
    expect(screen.getByText("Measuring this machine…")).toBeTruthy();
  });

  it("shows the machine's usage and each app's share, charted over 24 hours", async () => {
    await renderOnline();
    await sendMetrics(sample);
    const cpu = screen.getByRole("region", { name: "CPU" });
    expect(within(cpu).getByText("12.5%")).toBeTruthy();
    expect(within(cpu).getByText("8 CPUs · load 0.50 0.25 0.75")).toBeTruthy();
    const memory = screen.getByRole("region", { name: "Memory" });
    expect(within(memory).getByText("4.0 GiB of 16.0 GiB")).toBeTruthy();
    const disk = screen.getByRole("region", { name: "Disk" });
    expect(within(disk).getByText("Read 2.0 KiB/s · write –")).toBeTruthy();
    const network = screen.getByRole("region", { name: "Network" });
    expect(within(network).getByText("In 1.5 KiB/s")).toBeTruthy();
    expect(screen.queryByText(/not the virtual machine/)).toBeNull();

    const chart = await screen.findByRole("img", { name: "CPU over the last 24 hours" });
    expect(chart.querySelector("path")?.getAttribute("d")).toBe("M97.22 26.25 L98.61 26.25");
    expect(historyRequests).toEqual(["/api/metrics/history?range=24h"]);

    const apps = screen.getByRole("region", { name: "Apps on this machine" });
    const row = within(apps).getByRole("row", { name: /blog/ });
    expect(within(row).getByText("3.5%")).toBeTruthy();
    expect(within(row).getByText("300.0 MiB")).toBeTruthy();
    expect(within(row).getByRole("img", { name: "blog CPU over the last 24 hours" })).toBeTruthy();
  });

  it("links a project to the app it belongs to, even under another name", async () => {
    await renderOnline();
    const app: App = {
      name: "journal",
      status: "running",
      dir: "/home/me/Alchemy/apps/journal",
      root: "/home/me/Alchemy/apps",
      compose_file: "compose.yaml",
      project: "blog",
      images: "registry",
      services: [],
      problems: [],
      stop_reason: { kind: "running" },
    };
    const apps: AppList = { apps: [app], unmanaged: [], dirs: [] };
    await act(async () => {
      source().emit("apps", JSON.stringify(apps));
      await new Promise((resolve) => setTimeout(resolve, 0));
    });
    const other = { ...sample.projects[0]!, name: "legacy" };
    await sendMetrics({ ...sample, projects: [...sample.projects, other] });

    const table = screen.getByRole("region", { name: "Apps on this machine" });
    const link = within(table).getByRole("link", { name: "blog" });
    expect(link.getAttribute("href")).toBe("/apps/journal");
    const legacy = within(table).getByRole("row", { name: /legacy/ });
    expect(within(legacy).queryByRole("link")).toBeNull();
  });

  it("says when the figures are the host's, not Docker's VM", async () => {
    await renderOnline();
    await sendMetrics({ ...sample, containers_in_vm: true, projects: [] });
    expect(screen.getByText(/not the virtual machine Docker runs your apps in/)).toBeTruthy();
    expect(screen.getByText("No apps are running.")).toBeTruthy();
  });

  it("says when alchemyd is too old to report usage", async () => {
    oldDaemon = true;
    await renderOnline();
    expect(await screen.findByText(/doesn't report usage/)).toBeTruthy();
  });

  it("switches the history range", async () => {
    await renderOnline();
    await sendMetrics(sample);
    await act(async () => {
      screen.getByRole("button", { name: "1h" }).click();
    });
    expect(await screen.findByRole("img", { name: "CPU over the last hour" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "1h" }).getAttribute("aria-pressed")).toBe("true");
    expect(historyRequests).toContain("/api/metrics/history?range=1h");
  });

  it("lists every port and who holds it, linking apps", async () => {
    const tcp = (published: string, host_ip: string | null = null) => ({
      protocol: "tcp" as const,
      host_ip,
      published,
      target: null,
    });
    portList = {
      ports: [
        {
          port: tcp("3000"),
          holder: { kind: "app", app: "wiki", service: "app" },
          source: "declared",
        },
        {
          port: tcp("8080"),
          holder: { kind: "app", app: "blog", service: "web" },
          source: "live",
        },
        {
          port: tcp("8443"),
          holder: { kind: "reservation", owner: "proxy" },
          source: "reserved",
        },
        {
          port: tcp("9000", "0.0.0.0"),
          holder: { kind: "program", process: "nc", pid: 42 },
          source: "live",
        },
      ],
      warnings: ["alchemyd couldn't read which programs are listening on this machine."],
    };
    await renderOnline();
    const blog: App = {
      name: "blog",
      status: "running",
      dir: "/home/me/Alchemy/apps/blog",
      root: "/home/me/Alchemy/apps",
      compose_file: "compose.yaml",
      project: "blog",
      images: "registry",
      services: [],
      problems: [],
      stop_reason: { kind: "running" },
    };
    await act(async () => {
      source().emit("apps", JSON.stringify({ apps: [blog], unmanaged: [], dirs: [] }));
      await new Promise((resolve) => setTimeout(resolve, 0));
    });

    const ports = await screen.findByRole("region", { name: "Ports" });
    const live = await within(ports).findByRole("row", { name: /8080\/tcp/ });
    expect(within(live).getByText("In use")).toBeTruthy();
    expect(within(live).getByRole("link", { name: "blog" }).getAttribute("href")).toBe(
      "/apps/blog",
    );
    const declared = within(ports).getByRole("row", { name: /3000\/tcp/ });
    expect(within(declared).getByText("Declared")).toBeTruthy();
    expect(within(declared).getByText("app of wiki")).toBeTruthy();
    expect(within(declared).queryByRole("link")).toBeNull();
    const reserved = within(ports).getByRole("row", { name: /8443\/tcp/ });
    expect(within(reserved).getByText("Reserved for proxy")).toBeTruthy();
    const nc = within(ports).getByRole("row", { name: /9000\/tcp/ });
    expect(within(nc).getByText("0.0.0.0")).toBeTruthy();
    expect(within(nc).getByText("nc (pid 42)")).toBeTruthy();
    expect(within(ports).getByText(/couldn't read which programs are listening/)).toBeTruthy();
    expect(
      within(ports).getByText(/Declared ports belong to apps that aren't running/),
    ).toBeTruthy();
  });

  it("says when alchemyd is too old to list ports", async () => {
    oldDaemon = true;
    await renderOnline();
    const ports = screen.getByRole("region", { name: "Ports" });
    expect(await within(ports).findByText(/doesn't list ports/)).toBeTruthy();
  });
});
