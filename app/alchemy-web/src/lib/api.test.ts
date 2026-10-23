import { afterEach, beforeEach, describe, expect, it, vi } from "vite-plus/test";

import { type ApiEvent, type EngineStatus, api, subscribe } from "./api.ts";

/** Minimal EventSource stand-in; jsdom does not implement one. */
class FakeEventSource extends EventTarget {
  static instances: FakeEventSource[] = [];
  readonly url: string;
  closed = false;

  constructor(url: string | URL) {
    super();
    this.url = String(url);
    FakeEventSource.instances.push(this);
  }

  /** Deliver one SSE frame, as the browser would. */
  emit(event: string, data: string): void {
    this.dispatchEvent(new MessageEvent(event, { data }));
  }

  close(): void {
    this.closed = true;
  }
}

const ready: EngineStatus = {
  state: "ready",
  flavor: "docker_desktop",
  engine_version: "29.8.1",
  api_version: "1.52",
  compose_version: "2.40.0",
  desktop_version: "4.93.0",
  endpoint: "unix:///var/run/docker.sock",
};

function lastSource(): FakeEventSource {
  const source = FakeEventSource.instances.at(-1);
  if (!source) throw new Error("no EventSource opened");
  return source;
}

beforeEach(() => {
  FakeEventSource.instances = [];
  vi.stubGlobal("EventSource", FakeEventSource);
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("api.engine", () => {
  it("fetches the engine status", async () => {
    const fetch = vi.fn(async () => new Response(JSON.stringify(ready)));
    vi.stubGlobal("fetch", fetch);
    await expect(api.engine()).resolves.toEqual(ready);
    expect(fetch).toHaveBeenCalledWith("/api/engine", { signal: undefined });
  });

  it("rejects on an error status", async () => {
    vi.stubGlobal("fetch", async () => new Response("{}", { status: 500 }));
    await expect(api.engine()).rejects.toThrow("GET /api/engine failed: 500");
  });
});

describe("subscribe", () => {
  it("opens the event stream and delivers engine events", () => {
    const events: ApiEvent[] = [];
    subscribe((e) => events.push(e));

    const source = lastSource();
    expect(source.url).toBe("/api/events");

    source.emit("engine", JSON.stringify(ready));
    source.emit("engine", JSON.stringify({ ...ready, state: "not_running" }));
    expect(events).toEqual([
      { event: "engine", data: ready },
      { event: "engine", data: { ...ready, state: "not_running" } },
    ]);
  });

  it("skips malformed frames and unknown events", () => {
    const onEvent = vi.fn();
    subscribe(onEvent);
    const source = lastSource();
    source.emit("engine", "not json");
    source.emit("stacks", JSON.stringify([]));
    expect(onEvent).not.toHaveBeenCalled();
  });

  it("reports open and error", () => {
    const onOpen = vi.fn();
    const onError = vi.fn();
    subscribe(() => {}, { onOpen, onError });
    const source = lastSource();
    source.dispatchEvent(new Event("open"));
    source.dispatchEvent(new Event("error"));
    expect(onOpen).toHaveBeenCalledOnce();
    expect(onError).toHaveBeenCalledOnce();
  });

  it("closes the stream when unsubscribed", () => {
    const onEvent = vi.fn();
    const unsubscribe = api.subscribe(onEvent);
    const source = lastSource();
    expect(source.closed).toBe(false);
    unsubscribe();
    expect(source.closed).toBe(true);
  });
});
