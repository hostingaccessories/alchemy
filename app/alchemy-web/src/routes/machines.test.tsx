import { act, cleanup, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vite-plus/test";

import type { EngineStatus } from "../lib/api.ts";
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

function source(): FakeEventSource {
  const last = FakeEventSource.instances.at(-1);
  if (!last) throw new Error("no EventSource opened");
  return last;
}

function sendEngine(status: EngineStatus) {
  act(() => {
    source().dispatchEvent(new MessageEvent("engine", { data: JSON.stringify(status) }));
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
    sendEngine(status);
    for (const text of texts) {
      expect(within(docker()).getByText(text)).toBeTruthy();
    }
  });

  it("shows Starting with a labelled spinner", async () => {
    await renderOnline();
    sendEngine({ ...unknown, state: "starting" });
    expect(within(docker()).getByRole("img", { name: "In progress" })).toBeTruthy();
  });

  it("follows engine changes live", async () => {
    await renderOnline();
    sendEngine(ready);
    expect(within(docker()).getByText("Docker is ready")).toBeTruthy();

    sendEngine({ ...unknown, flavor: "docker_desktop" });
    expect(
      within(docker()).getByText("Docker Desktop isn't running. Open it to continue."),
    ).toBeTruthy();
    expect(within(docker()).queryByText("Docker is ready")).toBeNull();

    sendEngine({ ...unknown, state: "starting", flavor: "docker_desktop" });
    expect(within(docker()).getByText("Docker Desktop is starting…")).toBeTruthy();

    sendEngine(ready);
    expect(within(docker()).getByText("Docker is ready")).toBeTruthy();
    expect(FakeEventSource.instances).toHaveLength(1);
  });

  it("falls back to offline when the stream drops and the daemon is gone, then recovers", async () => {
    await renderOnline();
    sendEngine(ready);

    daemonUp = false;
    dispatch("error");
    expect(await screen.findByText("alchemyd is not reachable")).toBeTruthy();
    expect(screen.queryByRole("status", { name: "Docker" })).toBeNull();

    daemonUp = true;
    dispatch("open");
    expect(await screen.findByText("alchemyd 9.9.9")).toBeTruthy();
    expect(within(docker()).getByText("Checking Docker…")).toBeTruthy();
    sendEngine({ ...unknown, flavor: "docker_desktop" });
    expect(
      within(docker()).getByText("Docker Desktop isn't running. Open it to continue."),
    ).toBeTruthy();
  });

  it("stays online while the stream reconnects to a reachable daemon", async () => {
    await renderOnline();
    sendEngine(ready);
    dispatch("error");
    expect(within(docker()).getByText("Checking Docker…")).toBeTruthy();
    await act(async () => {});
    expect(screen.getByText("alchemyd 9.9.9")).toBeTruthy();
    sendEngine(ready);
    expect(within(docker()).getByText("Docker is ready")).toBeTruthy();
  });

  it("closes the stream on unmount", async () => {
    const { unmount } = await renderMachines();
    await screen.findByText("alchemyd 9.9.9");
    const opened = source();
    expect(opened.closed).toBe(false);
    unmount();
    expect(opened.closed).toBe(true);
  });
});
