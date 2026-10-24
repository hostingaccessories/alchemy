import { act, cleanup, fireEvent, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vite-plus/test";

import type { App, AppList, LogLine } from "../lib/api.ts";
import { FakeEventSource, renderAt } from "../test/render.tsx";

const container = (id: string, name: string) => ({
  id,
  name,
  image: null,
  state: "running" as const,
  health: null,
});

const blog: App = {
  name: "blog",
  status: "running",
  dir: "/home/me/Alchemy/apps/blog",
  root: "/home/me/Alchemy/apps",
  compose_file: "compose.yaml",
  project: "blog",
  images: "registry",
  services: [
    { name: "web", containers: [container("w1", "blog-web-1")] },
    { name: "db", containers: [container("d1", "blog-db-1")] },
  ],
  problems: [],
};

const apps: AppList = { apps: [blog], unmanaged: [], dirs: [] };

const writeText = vi.fn<(text: string) => Promise<void>>();

beforeEach(() => {
  FakeEventSource.instances = [];
  vi.stubGlobal("EventSource", FakeEventSource);
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string) => {
      if (url === "/api/apps") {
        return new Response(JSON.stringify(apps));
      }
      if (url === "/api/metrics") {
        return new Response(JSON.stringify({ projects: [] }));
      }
      if (url === "/api/operations") {
        return new Response(JSON.stringify({ operations: [] }));
      }
      return new Response(JSON.stringify({ version: "9.9.9", api: 1 }));
    }),
  );
  writeText.mockReset();
  writeText.mockResolvedValue(undefined);
  Object.defineProperty(navigator, "clipboard", { value: { writeText }, configurable: true });
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  Reflect.deleteProperty(navigator, "clipboard");
});

/** Every logs stream opened so far, oldest first. */
const logStreams = () => FakeEventSource.instances.filter((s) => s.url.includes("/logs"));

/** The logs stream opened last. */
function logs(): FakeEventSource {
  const source = logStreams().at(-1);
  if (!source) throw new Error("no logs stream opened");
  return source;
}

function line(service: string, text: string, stream: LogLine["stream"] = "stdout"): LogLine {
  return {
    service,
    container: `blog-${service}-1`,
    stream,
    time: "2024-05-06T07:08:09.000000000Z",
    line: text,
  };
}

function emit(event: string, data: unknown) {
  act(() => logs().emit(event, JSON.stringify(data)));
}

/** The browser connected the latest logs stream. */
function connected() {
  act(() => {
    logs().dispatchEvent(new Event("open"));
  });
}

async function openPage() {
  await renderAt("/apps/blog");
  await screen.findByRole("heading", { name: "Logs" });
  connected();
}

const view = () => screen.getByRole("log", { name: "Log lines" });

describe("App logs", () => {
  it("shows the tail, then follows new lines from every service", async () => {
    await openPage();
    expect(logs().url).toBe("/api/apps/blog/logs?follow=true");
    expect(screen.getByText("Loading logs…")).toBeTruthy();

    emit("log", line("web", "GET / 200"));
    emit("log", line("db", "checkpoint", "stderr"));
    emit("following", {});
    emit("log", line("web", "GET /about 200"));

    const text = view().textContent;
    expect(text).toContain("web | GET / 200");
    expect(text).toContain("db | checkpoint");
    expect(text).toContain("web | GET /about 200");
    expect(text!.indexOf("GET / 200")).toBeLessThan(text!.indexOf("GET /about"));
  });

  it("shows one service's lines without the service name", async () => {
    await openPage();
    const first = logs();
    fireEvent.change(screen.getByRole("combobox", { name: "Service" }), {
      target: { value: "db" },
    });
    expect(first.closed).toBe(true);
    expect(logs().url).toBe("/api/apps/blog/logs?service=db&follow=true");
    connected();
    emit("log", line("db", "ready"));
    expect(view().textContent).toBe("ready");
  });

  it("reads the tail once when not following, and says when there's nothing", async () => {
    await openPage();
    fireEvent.click(screen.getByRole("checkbox", { name: "Follow new lines" }));
    expect(logs().url).toBe("/api/apps/blog/logs");
    connected();
    emit("done", { error: null });
    expect(logs().closed).toBe(true);
    expect(view().textContent).toBe("No log lines.");
  });

  it("says when following has found no lines yet", async () => {
    await openPage();
    emit("following", {});
    expect(view().textContent).toContain("No log lines yet");
  });

  it("shows why the logs couldn't be read, and tries again", async () => {
    await openPage();
    emit("done", { error: "Docker isn't running" });
    const alert = screen.getByRole("alert");
    expect(alert.textContent).toContain("Couldn't read the logs");
    expect(alert.textContent).toContain("Docker isn't running");

    fireEvent.click(within(alert).getByRole("button", { name: "Try again" }));
    expect(logStreams()).toHaveLength(2);
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("starts over when the browser reconnects, since the tail is sent again", async () => {
    await openPage();
    emit("log", line("web", "one"));
    act(() => {
      logs().readyState = 0;
      logs().dispatchEvent(new Event("error"));
    });
    expect(screen.getByText(/Reconnecting/)).toBeTruthy();
    act(() => {
      logs().readyState = 1;
      logs().dispatchEvent(new Event("open"));
    });
    emit("log", line("web", "one"));
    expect(view().textContent).toBe("web | one");
  });

  it("stops following the newest line when scrolled up, and jumps back", async () => {
    await openPage();
    emit("log", line("web", "one"));
    const el = view();
    Object.defineProperty(el, "scrollHeight", { value: 1000, configurable: true });
    Object.defineProperty(el, "clientHeight", { value: 200, configurable: true });

    emit("log", line("web", "two"));
    expect(el.scrollTop).toBe(1000);

    el.scrollTop = 100;
    fireEvent.scroll(el);
    emit("log", line("web", "three"));
    expect(el.scrollTop).toBe(100);

    fireEvent.click(screen.getByRole("button", { name: "Jump to newest" }));
    expect(el.scrollTop).toBe(1000);
    expect(screen.queryByRole("button", { name: "Jump to newest" })).toBeNull();
  });

  it("copies the lines shown", async () => {
    await openPage();
    emit("log", line("web", "one"));
    emit("log", line("db", "two"));
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Copy logs" }));
    });
    expect(writeText).toHaveBeenCalledWith("web | one\ndb | two");
  });

  it("closes the stream when leaving the page", async () => {
    const { router } = await renderAt("/apps/blog");
    await screen.findByRole("heading", { name: "Logs" });
    const source = logs();
    await act(() => router.navigate({ to: "/apps" }));
    expect(source.closed).toBe(true);
  });
});
