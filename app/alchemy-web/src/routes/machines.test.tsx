import { act, cleanup, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vite-plus/test";

import type {
  App,
  AppList,
  EngineStatus,
  MetricsHistory,
  MetricsResponse,
  PortList,
  MdnsReport,
  NetworkReport,
  NetworkSettings,
  NetworkWarning,
  Privilege,
  PrivilegeAction,
  PrivilegeChange,
  PrivilegeList,
  ProxyListener,
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

/** Whether the stubbed daemon predates `/api/metrics` and `/api/network`. */
let oldDaemon = false;

/** What the stubbed `GET /api/ports` answers; `null` never answers. */
let portList: PortList | null = null;

const clear: NetworkReport = {
  engine_ready: true,
  lan: ["10.0.1.0/24"],
  routes: [],
  bridge: "172.17.0.0/16",
  pools: [{ base: "172.17.0.0/16", size: 16 }],
  pools_source: "defaults",
  networks: [],
  alchemy: { name: "alchemy", state: "ready", subnet: "172.18.0.0/24" },
  warnings: [],
};

/** What the stubbed `GET /api/network` answers. */
let network: NetworkReport = clear;

/** What the stubbed `GET /api/settings/network` answers. */
let networkSettings: NetworkSettings = { mdns: true };

/** The bodies of every `PUT /api/settings/network`, in order. */
let settingsPuts: unknown[] = [];

/** Whether the stubbed `PUT /api/settings/network` fails. */
let settingsPutFails = false;

function privilege(action: PrivilegeAction, extra: Partial<Privilege> = {}): Privilege {
  return {
    action,
    state: "not_needed",
    reason: null,
    message: null,
    command: null,
    restarts: action === "low_ports",
    last: null,
    ...extra,
  };
}

const NOTHING_NEEDED: PrivilegeList = {
  privileges: [privilege("low_ports"), privilege("firewall"), privilege("system_service")],
};

/** What the stubbed `GET /api/machine/privileges` answers. */
let privilegeList: PrivilegeList = NOTHING_NEEDED;

/** Every `POST /api/machine/privileges/{action}`, as its path and body, in order. */
let privilegePosts: { url: string; body: unknown }[] = [];

/** The stubbed answers to the next POSTs, in order; then `unchanged`. */
let privilegeAnswers: { status: number; body: unknown }[] = [];

/** Whether the stubbed daemon predates `/api/machine/privileges` but reports the rest. */
let noPrivileges = false;

/** Whether the stubbed daemon refuses everything but its version as signed out. */
let signedOut = false;

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
  network = clear;
  networkSettings = { mdns: true };
  settingsPuts = [];
  settingsPutFails = false;
  privilegeList = NOTHING_NEEDED;
  privilegePosts = [];
  privilegeAnswers = [];
  noPrivileges = false;
  signedOut = false;
  FakeEventSource.instances = [];
  vi.stubGlobal("EventSource", FakeEventSource);
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string, init?: RequestInit) => {
      if (!daemonUp) {
        throw new TypeError("connection refused");
      }
      if (signedOut && url !== "/api/version") {
        return new Response(JSON.stringify({ error: "not signed in", code: "unauthorized" }), {
          status: 401,
        });
      }
      if (
        ((oldDaemon || noPrivileges) && url.startsWith("/api/machine/")) ||
        (oldDaemon &&
          (url.startsWith("/api/metrics") ||
            url === "/api/ports" ||
            url === "/api/network" ||
            url === "/api/settings/network"))
      ) {
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
      if (url === "/api/network") {
        return new Response(JSON.stringify(network), { status: 200 });
      }
      if (url === "/api/settings/network" && init?.method === "PUT") {
        const update = (typeof init.body === "string" ? JSON.parse(init.body) : {}) as {
          mdns?: boolean;
        };
        settingsPuts.push(update);
        if (settingsPutFails) {
          return new Response(
            JSON.stringify({ error: "the settings store is read-only", code: "internal" }),
            { status: 500 },
          );
        }
        networkSettings = { mdns: update.mdns ?? networkSettings.mdns };
        network = {
          ...network,
          mdns: {
            state: networkSettings.mdns ? "starting" : "off",
            backend: null,
            message: null,
            fix: null,
          },
        };
        return new Response(JSON.stringify(networkSettings), { status: 200 });
      }
      if (url === "/api/settings/network") {
        return new Response(JSON.stringify(networkSettings), { status: 200 });
      }
      if (url === "/api/machine/privileges") {
        return new Response(JSON.stringify(privilegeList), { status: 200 });
      }
      if (url.startsWith("/api/machine/privileges/") && init?.method === "POST") {
        privilegePosts.push({
          url,
          body: typeof init.body === "string" ? JSON.parse(init.body) : null,
        });
        const answer = privilegeAnswers.shift() ?? {
          status: 200,
          body: {
            outcome: "unchanged",
            command: null,
            message: null,
            privilege: privilege("other"),
          },
        };
        return new Response(JSON.stringify(answer.body), { status: answer.status });
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

  const fix = [
    "Add these settings to /etc/docker/daemon.json (create it if it doesn't exist), then restart Docker with `sudo systemctl restart docker`:",
    "{",
    '  "bip": "172.18.0.1/24"',
    "}",
    "Networks that already exist keep their subnets until they are removed and created again.",
  ].join("\n");

  function warning(kind: NetworkWarning["kind"], message: string, fix: string | null) {
    return {
      kind,
      message,
      fix,
      subnet: "172.17.0.0/16",
      host: "lan" as const,
      host_subnet: "172.17.0.0/16",
      network: null,
    };
  }

  it("shows no network warnings when nothing overlaps", async () => {
    await renderOnline();
    await sendEngine(ready);
    expect(screen.queryByRole("group", { name: "Network warnings" })).toBeNull();
  });

  it("explains networks that overlap this machine's, with the fix", async () => {
    network = {
      ...clear,
      lan: ["172.17.0.0/16"],
      warnings: [
        warning(
          "bridge_overlap",
          "Docker's default bridge network (172.17.0.0/16) overlaps your local network 172.17.0.0/16, so containers on it can't reach devices on it.",
          fix,
        ),
        warning(
          "pool_overlap",
          "Docker's address pool 172.17.0.0/16 overlaps your local network 172.17.0.0/16, so a new network may be given addresses that hide devices on it.",
          fix,
        ),
        warning("create_failed", "Alchemy couldn't create its network alchemy.", null),
      ],
    };
    await renderOnline();
    const group = await screen.findByRole("group", { name: "Network warnings" });
    expect(within(group).getByText("Docker's default network overlaps your network")).toBeTruthy();
    expect(within(group).getByText("Docker may put new networks on top of yours")).toBeTruthy();
    expect(within(group).getByText("Alchemy couldn't create its network")).toBeTruthy();
    expect(
      within(group).getByText(/Add these settings to \/etc\/docker\/daemon.json/),
    ).toBeTruthy();
    const settings = group.querySelector("pre");
    expect(settings?.textContent).toBe('{\n  "bip": "172.18.0.1/24"\n}');
    expect(within(group).getAllByRole("button", { name: "Copy settings" })).toHaveLength(1);
    expect(within(group).getByText("The same change as above fixes this.")).toBeTruthy();
  });

  it("shows no network warnings when alchemyd is too old to report them", async () => {
    oldDaemon = true;
    await renderOnline();
    expect(await screen.findByText(/doesn't report usage/)).toBeTruthy();
    expect(screen.queryByRole("group", { name: "Network warnings" })).toBeNull();
  });

  const listener = (port: number, app: string | null): ProxyListener => ({
    port,
    app,
    state: "listening",
    message: null,
    fix: null,
  });

  it("shows no web address warnings while every listener works", async () => {
    network = {
      ...clear,
      proxy: {
        reachability: "container_ip",
        host: listener(80, null),
        ports: [listener(8100, "blog")],
      },
    };
    await renderOnline();
    await screen.findByText(/This machine/);
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 0));
    });
    expect(screen.queryByRole("group", { name: "Web address warnings" })).toBeNull();
  });

  it("warns when port 80 and an app's port can't be opened, with the fix", async () => {
    const setcap = 'sudo setcap cap_net_bind_service=+ep "$(command -v alchemyd)"';
    network = {
      ...clear,
      proxy: {
        reachability: "container_ip",
        host: {
          ...listener(80, null),
          state: "unavailable",
          message:
            "Alchemy doesn't have permission to listen on port 80, so addresses like http://<app>.localhost don't work. Each app's own port still does.",
          fix: `Let alchemyd use low ports with \`${setcap}\`, then restart it.`,
        },
        ports: [
          listener(8100, "blog"),
          {
            ...listener(8101, "wiki"),
            state: "unavailable",
            message: "Another program is using port 8101.",
          },
        ],
      },
    };
    await renderOnline();
    const group = await screen.findByRole("group", { name: "Web address warnings" });
    expect(within(group).getByText("Addresses on port 80 don't work")).toBeTruthy();
    expect(within(group).getByText(/doesn't have permission to listen on port 80/)).toBeTruthy();
    expect(within(group).getByText(setcap).tagName).toBe("CODE");
    expect(within(group).getByRole("button", { name: "Copy command" })).toBeTruthy();
    expect(within(group).getByText("wiki's address on port 8101 doesn't work")).toBeTruthy();
    expect(within(group).getByText("Another program is using port 8101.")).toBeTruthy();
    expect(within(group).queryByText(/8100/)).toBeNull();
  });

  it("says when another program takes port 80 although it is open", async () => {
    const taken =
      "Another program on this computer takes port 80. Your apps still work at their IP:port address.";
    network = {
      ...clear,
      proxy: {
        reachability: "published_only",
        host: {
          ...listener(80, null),
          state: "unavailable",
          message: taken,
          fix: "Stop the program or firewall rule that takes port 80; some development tools redirect it with pf on macOS. Alchemy checks the port again every few seconds.",
        },
        ports: [],
      },
    };
    await renderOnline();
    const group = await screen.findByRole("group", { name: "Web address warnings" });
    expect(within(group).getByText("Addresses on port 80 don't work")).toBeTruthy();
    expect(within(group).queryByText(/can't listen/)).toBeNull();
    expect(within(group).getByText(taken)).toBeTruthy();
    expect(within(group).getByText(/Stop the program or firewall rule/)).toBeTruthy();
    expect(within(group).queryByRole("button", { name: "Copy command" })).toBeNull();
  });

  function localNames() {
    return screen.getByRole("status", { name: "Local names" });
  }

  const mdns = (report: Partial<MdnsReport>): NetworkReport => ({
    ...clear,
    mdns: { state: "announcing", backend: null, message: null, fix: null, ...report },
  });

  const reports: [string, Partial<MdnsReport>, (string | RegExp)[]][] = [
    [
      "announcing through avahi",
      { backend: "avahi" },
      ["Devices on your network can open apps at their .local address", "Announced through Avahi."],
    ],
    ["announcing through Bonjour", { backend: "dns_sd" }, ["Announced through macOS's Bonjour."]],
    [
      "announcing with the built-in responder",
      { backend: "built_in" },
      ["Announced by Alchemy's built-in responder."],
    ],
    ["starting", { state: "starting" }, ["Alchemy is starting to announce your apps…"]],
    ["off", { state: "off" }, [/Local names are off/, /at their IP:port address/]],
    [
      "unavailable without a message",
      { state: "unavailable" },
      [
        "Alchemy can't announce your apps' .local names",
        "Your apps still work at their IP:port address.",
      ],
    ],
    [
      "in a state this build doesn't know",
      { state: "paused" as MdnsReport["state"] },
      ["Local names are in a state this version of Alchemy doesn't recognize. Update Alchemy."],
    ],
  ];

  it.each(reports)("shows local names %s", async (_name, report, texts) => {
    network = mdns(report);
    await renderOnline();
    await screen.findByRole("heading", { name: "Local names" });
    for (const text of texts) {
      expect(within(localNames()).getByText(text)).toBeTruthy();
    }
  });

  it("says why names can't be announced, with the fix", async () => {
    network = mdns({
      state: "unavailable",
      backend: "dns_sd",
      message:
        "macOS doesn't let Alchemy use your local network. Your apps still work at their IP:port address.",
      fix: "Allow alchemyd in System Settings › Privacy & Security › Local Network, then run `alchemy status`.",
    });
    await renderOnline();
    await screen.findByRole("heading", { name: "Local names" });
    const status = localNames();
    expect(within(status).getByText(/doesn't let Alchemy use your local network/)).toBeTruthy();
    expect(within(status).getByText("alchemy status").tagName).toBe("CODE");
    expect(within(status).queryByText(/Announced/)).toBeNull();
  });

  it("shows nothing about local names when alchemyd is too old to say", async () => {
    await renderOnline();
    await screen.findByText(/This machine/);
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 0));
    });
    expect(screen.queryByRole("heading", { name: "Local names" })).toBeNull();
  });

  it("turns local names off and on", async () => {
    network = mdns({ backend: "avahi" });
    await renderOnline();
    const toggle = await screen.findByRole("checkbox", { name: /Announce apps on your network/ });
    expect((toggle as HTMLInputElement).checked).toBe(true);

    await userEvent.setup().click(toggle);
    expect(await within(localNames()).findByText(/Local names are off/)).toBeTruthy();
    expect((toggle as HTMLInputElement).checked).toBe(false);
    expect(settingsPuts).toEqual([{ mdns: false }]);

    await userEvent.setup().click(toggle);
    expect(
      await within(localNames()).findByText("Alchemy is starting to announce your apps…"),
    ).toBeTruthy();
    expect((toggle as HTMLInputElement).checked).toBe(true);
    expect(settingsPuts).toEqual([{ mdns: false }, { mdns: true }]);
  });

  it("says when the setting couldn't be changed", async () => {
    network = mdns({ backend: "avahi" });
    settingsPutFails = true;
    await renderOnline();
    const toggle = await screen.findByRole("checkbox", { name: /Announce apps on your network/ });
    await userEvent.setup().click(toggle);
    const alert = await screen.findByRole("alert");
    expect(within(alert).getByText("Couldn't change local names")).toBeTruthy();
    expect(within(alert).getByText("the settings store is read-only")).toBeTruthy();
    expect((toggle as HTMLInputElement).checked).toBe(true);
  });
});

describe("Privileges", () => {
  const helper = "/home/me/alchemy/target/release/alchemy-helper";
  const grantLine = `sudo ${helper} grant-low-ports`;
  const revokeLine = `sudo ${helper} revoke-low-ports`;
  const terminal = (line: string) => ({ argv: line.split(" "), line });

  function list(...privileges: Privilege[]): PrivilegeList {
    const byAction = new Map(privileges.map((p) => [p.action, p]));
    return {
      privileges: NOTHING_NEEDED.privileges.map((p) => byAction.get(p.action) ?? p),
    };
  }

  function answer(status: number, change: Partial<PrivilegeChange>) {
    privilegeAnswers.push({
      status,
      body: {
        outcome: "unchanged",
        command: null,
        message: null,
        privilege: privilege("low_ports"),
        ...change,
      },
    });
  }

  /** Send a `privileges` event and wait for it to render. */
  async function sendPrivileges(update: PrivilegeList) {
    await act(async () => {
      source().emit("privileges", JSON.stringify(update));
      await new Promise((resolve) => setTimeout(resolve, 0));
    });
  }

  async function row(name: string) {
    await screen.findByRole("heading", { name: "Privileges" });
    return screen.findByRole("group", { name });
  }

  it("lists each action and its state", async () => {
    await renderOnline();
    expect(
      within(await row("Port 80")).getByText("Alchemy can use port 80 without extra permission"),
    ).toBeTruthy();
    expect(within(await row("Firewall")).getByText("No firewall needs opening")).toBeTruthy();
    expect(
      within(await row("System service")).getByText("alchemyd doesn't need a system service"),
    ).toBeTruthy();
    expect(screen.queryByRole("button", { name: /Allow|Open|Install|Remove|Close/ })).toBeNull();
  });

  it("shows nothing about privileges when alchemyd is too old to report them", async () => {
    oldDaemon = true;
    await renderOnline();
    expect(await screen.findByText(/doesn't report usage/)).toBeTruthy();
    expect(screen.queryByRole("heading", { name: "Privileges" })).toBeNull();
  });

  it("asks for the system prompt, waits while it's up, and says when it was cancelled", async () => {
    privilegeList = list(privilege("low_ports", { state: "needed" }));
    answer(202, { outcome: "pending", privilege: privilege("low_ports", { state: "pending" }) });
    await renderOnline();
    const port = await row("Port 80");
    expect(within(port).getByText("Alchemy needs permission to use port 80")).toBeTruthy();
    expect(within(port).getByText(/which signs this browser out/)).toBeTruthy();

    await userEvent.setup().click(within(port).getByRole("button", { name: "Allow port 80" }));
    expect(privilegePosts).toEqual([
      { url: "/api/machine/privileges/low_ports", body: { grant: true } },
    ]);
    await sendPrivileges(list(privilege("low_ports", { state: "pending" })));
    expect(within(port).getByText("Waiting for you to answer the system prompt…")).toBeTruthy();
    expect(within(port).getByRole("img", { name: "In progress" })).toBeTruthy();
    expect(within(port).queryByRole("button")).toBeNull();

    await sendPrivileges(
      list(
        privilege("low_ports", {
          state: "needed",
          last: { grant: true, result: "cancelled", message: null, at: 1 },
        }),
      ),
    );
    expect(within(port).getByText("The last try was cancelled at the system prompt.")).toBeTruthy();
    expect(within(port).getByRole("button", { name: "Allow port 80" })).toBeTruthy();
  });

  it("says why the last try failed", async () => {
    privilegeList = list(
      privilege("firewall", {
        state: "needed",
        last: { grant: true, result: "failed", message: "alchemy-helper: ufw exited 1", at: 1 },
      }),
    );
    await renderOnline();
    const firewall = await row("Firewall");
    expect(
      within(firewall).getByText("The firewall may block other devices from your apps"),
    ).toBeTruthy();
    expect(
      within(firewall).getByText("The last try failed: alchemy-helper: ufw exited 1"),
    ).toBeTruthy();
    expect(within(firewall).getByRole("button", { name: "Open the firewall" })).toBeTruthy();
  });

  it("shows the command to run when there's no prompt, and tries again", async () => {
    privilegeList = list(
      privilege("low_ports", {
        state: "needs_terminal",
        reason: "untrusted_helper",
        message: `alchemy-helper isn't installed safely: ${helper} is owned by UID 501, not root.`,
        command: terminal(grantLine),
      }),
    );
    for (let i = 0; i < 2; i++) {
      answer(200, {
        outcome: "needs_terminal",
        command: terminal(grantLine),
        message: "alchemy-helper isn't installed safely.",
      });
    }
    await renderOnline();
    const port = await row("Port 80");
    expect(within(port).getByText("Alchemy needs permission to use port 80")).toBeTruthy();
    expect(within(port).getByText(/isn't installed safely: /)).toBeTruthy();
    expect(port.querySelector("pre")?.textContent).toBe(grantLine);
    expect(within(port).getByRole("button", { name: "Copy command" })).toBeTruthy();
    expect(within(port).queryByRole("button", { name: "Allow port 80" })).toBeNull();

    const user = userEvent.setup();
    await user.click(within(port).getByRole("button", { name: "Try again" }));
    await act(async () => {});
    expect(within(port).queryByText(/doesn't see the change yet/)).toBeNull();
    await user.click(within(port).getByRole("button", { name: "Try again" }));
    expect(await within(port).findByText(/doesn't see the change yet/)).toBeTruthy();
    expect(privilegePosts.map((p) => p.body)).toEqual([{ grant: true }, { grant: true }]);
  });

  it("restarts alchemyd for a change made in a terminal, then reconnects", async () => {
    privilegeList = list(privilege("low_ports", { state: "needs_restart" }));
    answer(202, { outcome: "restarting", privilege: privilege("low_ports", { state: "pending" }) });
    await renderOnline();
    const port = await row("Port 80");
    expect(within(port).getByText("Port 80 is allowed once alchemyd restarts")).toBeTruthy();
    await userEvent.setup().click(within(port).getByRole("button", { name: "Restart alchemyd" }));
    expect(privilegePosts).toEqual([
      { url: "/api/machine/privileges/low_ports", body: { grant: true } },
    ]);
    await sendPrivileges(list(privilege("low_ports", { state: "pending" })));
    expect(
      within(await row("Port 80")).getByText("Restarting alchemyd to put the change in effect…"),
    ).toBeTruthy();

    daemonUp = false;
    dispatch("error");
    expect(
      await screen.findByText("alchemyd is restarting to put the change in effect…"),
    ).toBeTruthy();
    expect(screen.queryByText("Alchemy can't reach alchemyd")).toBeNull();

    daemonUp = true;
    privilegeList = list(privilege("low_ports", { state: "granted" }));
    dispatch("open");
    await screen.findByText("alchemyd 9.9.9");
    await sendPrivileges(privilegeList);
    expect(within(await row("Port 80")).getByText("Alchemy can use port 80")).toBeTruthy();
    expect(screen.queryByText("alchemyd is restarting to put the change in effect…")).toBeNull();
  });

  it("calls a prompted change that succeeded a restart, before alchemyd goes", async () => {
    privilegeList = list(privilege("low_ports", { state: "needed" }));
    await renderOnline();
    await row("Port 80");
    await sendPrivileges(
      list(
        privilege("low_ports", {
          state: "pending",
          last: { grant: true, result: "done", message: null, at: 1 },
        }),
      ),
    );
    expect(
      within(await row("Port 80")).getByText("Restarting alchemyd to put the change in effect…"),
    ).toBeTruthy();
    daemonUp = false;
    dispatch("error");
    expect(
      await screen.findByText("alchemyd is restarting to put the change in effect…"),
    ).toBeTruthy();
  });

  it("tells a browser to sign in again after the restart", async () => {
    privilegeList = list(privilege("low_ports", { state: "needs_restart" }));
    answer(202, { outcome: "restarting", privilege: privilege("low_ports", { state: "pending" }) });
    await renderOnline();
    await userEvent
      .setup()
      .click(within(await row("Port 80")).getByRole("button", { name: "Restart alchemyd" }));
    await act(async () => {});
    signedOut = true;
    dispatch("error");
    const alert = await screen.findByRole("alert");
    expect(
      within(alert).getByText(
        "alchemyd restarted to put the change in effect, so you're signed out",
      ),
    ).toBeTruthy();
    expect(within(alert).getByText("alchemy open")).toBeTruthy();
    await waitFor(() => expect(screen.queryByRole("heading", { name: "Privileges" })).toBeNull());
  });

  it("undoes a granted action, through a terminal when it has to", async () => {
    privilegeList = list(privilege("low_ports", { state: "granted" }));
    answer(200, {
      outcome: "needs_terminal",
      command: terminal(revokeLine),
      message: "No polkit agent answered.",
    });
    answer(202, { outcome: "restarting", privilege: privilege("low_ports", { state: "pending" }) });
    await renderOnline();
    const port = await row("Port 80");
    expect(within(port).getByText("Alchemy can use port 80")).toBeTruthy();
    const user = userEvent.setup();
    await user.click(within(port).getByRole("button", { name: "Remove port 80 access" }));
    expect(await within(port).findByText("No polkit agent answered.")).toBeTruthy();
    expect(port.querySelector("pre")?.textContent).toBe(revokeLine);
    expect(within(port).queryByRole("button", { name: "Remove port 80 access" })).toBeNull();

    await user.click(within(port).getByRole("button", { name: "Try again" }));
    expect(privilegePosts.map((p) => p.body)).toEqual([{ grant: false }, { grant: false }]);
    await waitFor(() => expect(port.querySelector("pre")).toBeNull());
  });

  it("explains an action that can't be done", async () => {
    privilegeList = list(
      privilege("low_ports", {
        state: "unsupported",
        reason: "nosuid_mount",
        message: "alchemyd is on a filesystem mounted nosuid.",
      }),
    );
    await renderOnline();
    const port = await row("Port 80");
    expect(within(port).getByText("Alchemy can't be allowed port 80 on this machine")).toBeTruthy();
    expect(within(port).getByText("alchemyd is on a filesystem mounted nosuid.")).toBeTruthy();
    expect(within(port).queryByRole("button")).toBeNull();
  });

  it("says when alchemyd refused the change", async () => {
    privilegeList = list(privilege("firewall", { state: "needed" }));
    privilegeAnswers.push({
      status: 422,
      body: { error: "raw nftables rules are never edited", code: "unsupported" },
    });
    await renderOnline();
    const firewall = await row("Firewall");
    await userEvent
      .setup()
      .click(within(firewall).getByRole("button", { name: "Open the firewall" }));
    const alert = await within(firewall).findByRole("alert");
    expect(within(alert).getByText("Alchemy couldn't make the change")).toBeTruthy();
    expect(within(alert).getByText("raw nftables rules are never edited")).toBeTruthy();
    expect(privilegePosts).toEqual([
      { url: "/api/machine/privileges/firewall", body: { grant: true } },
    ]);
  });

  const denied: ProxyListener = {
    port: 80,
    app: null,
    state: "unavailable",
    message:
      "Alchemy doesn't have permission to listen on port 80, so addresses like http://<app>.localhost don't work.",
    fix: "Allow Alchemy to use port 80 with `alchemy machine privileges grant low-ports`, or from the machine page.",
    privilege: "low_ports",
  };

  function warnings() {
    return screen.findByRole("group", { name: "Web address warnings" });
  }

  it("offers Allow port 80 on the port 80 warning instead of a command", async () => {
    network = { ...clear, proxy: { reachability: "container_ip", host: denied, ports: [] } };
    privilegeList = list(privilege("low_ports", { state: "needed" }));
    answer(202, { outcome: "pending", privilege: privilege("low_ports", { state: "pending" }) });
    await renderOnline();
    const group = await warnings();
    expect(within(group).getByText("Addresses on port 80 don't work")).toBeTruthy();
    expect(within(group).getByText(/doesn't have permission to listen on port 80/)).toBeTruthy();
    const allow = await within(group).findByRole("button", { name: "Allow port 80" });
    expect(within(group).queryByText(/alchemy machine privileges/)).toBeNull();
    expect(within(group).queryByRole("button", { name: "Copy command" })).toBeNull();

    await userEvent.setup().click(allow);
    expect(privilegePosts).toEqual([
      { url: "/api/machine/privileges/low_ports", body: { grant: true } },
    ]);
    await sendPrivileges(list(privilege("low_ports", { state: "pending" })));
    expect(within(group).getByText("Waiting for you to answer the system prompt…")).toBeTruthy();
    expect(
      within(await row("Port 80")).getByText("Waiting for you to answer the system prompt…"),
    ).toBeTruthy();
  });

  it("shows the command on the port 80 warning when there's no prompt", async () => {
    network = { ...clear, proxy: { reachability: "container_ip", host: denied, ports: [] } };
    privilegeList = list(
      privilege("low_ports", {
        state: "needs_terminal",
        reason: "untrusted_helper",
        message: "alchemy-helper isn't installed safely.",
        command: terminal(grantLine),
      }),
    );
    await renderOnline();
    const group = await warnings();
    expect((await within(group).findByText(grantLine)).tagName).toBe("PRE");
    expect(within(group).getByRole("button", { name: "Try again" })).toBeTruthy();
    expect(within(group).queryByRole("button", { name: "Allow port 80" })).toBeNull();
  });

  it("keeps the fix's command when alchemyd can't report privileges", async () => {
    network = { ...clear, proxy: { reachability: "container_ip", host: denied, ports: [] } };
    noPrivileges = true;
    await renderOnline();
    const group = await warnings();
    expect(within(group).getByText("alchemy machine privileges grant low-ports").tagName).toBe(
      "CODE",
    );
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 0));
    });
    expect(within(group).queryByRole("button", { name: "Allow port 80" })).toBeNull();
  });

  it("refetches the web address report when the privileges change", async () => {
    network = { ...clear, proxy: { reachability: "container_ip", host: denied, ports: [] } };
    privilegeList = list(privilege("low_ports", { state: "needed" }));
    await renderOnline();
    await warnings();
    network = {
      ...clear,
      proxy: {
        reachability: "container_ip",
        host: { ...denied, state: "listening", message: null, fix: null, privilege: null },
        ports: [],
      },
    };
    await sendPrivileges(list(privilege("low_ports", { state: "granted" })));
    await waitFor(() =>
      expect(screen.queryByRole("group", { name: "Web address warnings" })).toBeNull(),
    );
  });
});
