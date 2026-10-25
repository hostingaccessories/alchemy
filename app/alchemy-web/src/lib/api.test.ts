import { afterEach, beforeEach, describe, expect, it, vi } from "vite-plus/test";

import { FakeEventSource } from "../test/event-source.ts";
import {
  type ApiEvent,
  ApiError,
  type EngineStatus,
  NO_RESPONSE,
  RESUBSCRIBE_DELAY_MS,
  type OperationEvent,
  api,
  followOperation,
  isSignedOut,
  request,
  subscribe,
} from "./api.ts";
import { forgetDaemonToken } from "./platform.ts";

const tauri = vi.hoisted(() => ({
  invoke: vi.fn(async (_command: string): Promise<string> => ""),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: tauri.invoke }));

const ready: EngineStatus = {
  state: "ready",
  flavor: "docker_desktop",
  engine_version: "29.8.1",
  api_version: "1.52",
  compose_version: "2.40.0",
  desktop_version: "4.93.0",
  endpoint: "unix:///var/run/docker.sock",
};

const lastSource = () => FakeEventSource.last();

beforeEach(() => {
  FakeEventSource.instances = [];
  vi.stubGlobal("EventSource", FakeEventSource);
});

afterEach(() => {
  vi.unstubAllGlobals();
});

async function rejection(promise: Promise<unknown>): Promise<unknown> {
  try {
    await promise;
  } catch (err) {
    return err;
  }
  throw new Error("expected a rejection");
}

describe("request", () => {
  it("GETs JSON from /api", async () => {
    const fetch = vi.fn(async () => new Response(JSON.stringify(ready)));
    vi.stubGlobal("fetch", fetch);
    await expect(api.engine()).resolves.toEqual(ready);
    expect(fetch).toHaveBeenCalledWith("/api/engine", {
      method: "GET",
      signal: undefined,
      headers: {},
    });
  });

  it("narrows metrics to an app by folder name or to a project", async () => {
    const fetch = vi.fn(async () => new Response(JSON.stringify({})));
    vi.stubGlobal("fetch", fetch);
    await api.metrics();
    await api.metrics(undefined, { app: "my blog" });
    await api.metricsHistory("24h");
    await api.metricsHistory("24h", { app: "blog" });
    await api.metricsHistory("1h", { project: "my-blog" });
    expect(fetch.mock.calls.map((call: unknown[]) => call[0])).toEqual([
      "/api/metrics",
      "/api/metrics?app=my+blog",
      "/api/metrics/history?range=24h",
      "/api/metrics/history?range=24h&app=blog",
      "/api/metrics/history?range=1h&project=my-blog",
    ]);
  });

  it("reads privileges and asks for one with a 202 answer", async () => {
    const pending = {
      outcome: "pending",
      command: null,
      message: null,
      privilege: {
        action: "low_ports",
        state: "pending",
        reason: null,
        message: null,
        command: null,
        restarts: true,
        last: null,
      },
    };
    const fetch = vi.fn(async (url: string) =>
      url.endsWith("/privileges")
        ? new Response(JSON.stringify({ privileges: [pending.privilege] }))
        : new Response(JSON.stringify(pending), { status: 202 }),
    );
    vi.stubGlobal("fetch", fetch);
    await expect(api.privileges()).resolves.toEqual({ privileges: [pending.privilege] });
    await expect(api.changePrivilege("low_ports", { grant: true })).resolves.toEqual(pending);
    expect(fetch).toHaveBeenLastCalledWith("/api/machine/privileges/low_ports", {
      method: "POST",
      signal: undefined,
      headers: { "content-type": "application/json" },
      body: '{"grant":true}',
    });
  });

  it("reads the last reconcile and starts Docker with an empty body", async () => {
    const status = {
      last: {
        trigger: "wake",
        at: 1_700_000_000,
        reason: "was running before the computer slept",
        apps: [{ app: "blog", outcome: "starting", operation: "op1", reason: null, message: null }],
      },
    };
    const starting = { result: "starting", message: "Docker Desktop is starting", command: null };
    const fetch = vi.fn(async (url: string) =>
      url.endsWith("/reconcile")
        ? new Response(JSON.stringify(status))
        : new Response(JSON.stringify(starting), { status: 202 }),
    );
    vi.stubGlobal("fetch", fetch);
    await expect(api.reconcile()).resolves.toEqual(status);
    await expect(api.startEngine()).resolves.toEqual(starting);
    expect(fetch).toHaveBeenLastCalledWith("/api/engine/start", {
      method: "POST",
      signal: undefined,
      headers: { "content-type": "application/json" },
      body: "{}",
    });
  });

  it.each(["POST", "PUT", "DELETE"] as const)("sends %s with a JSON body", async (method) => {
    const fetch = vi.fn(async () => new Response(JSON.stringify({ ok: true })));
    vi.stubGlobal("fetch", fetch);
    await expect(request(method, "/things/1", { body: { name: "web" } })).resolves.toEqual({
      ok: true,
    });
    expect(fetch).toHaveBeenCalledWith("/api/things/1", {
      method,
      signal: undefined,
      headers: { "content-type": "application/json" },
      body: '{"name":"web"}',
    });
  });

  it.each(["POST", "PUT", "DELETE"] as const)("sends %s without a body as JSON", async (method) => {
    const fetch = vi.fn(async () => new Response(null, { status: 204 }));
    vi.stubGlobal("fetch", fetch);
    await request(method, "/things/1");
    expect(fetch).toHaveBeenCalledWith("/api/things/1", {
      method,
      signal: undefined,
      headers: { "content-type": "application/json" },
    });
  });

  it("resolves to undefined for 204 No Content", async () => {
    vi.stubGlobal("fetch", async () => new Response(null, { status: 204 }));
    await expect(request("DELETE", "/things/1")).resolves.toBeUndefined();
  });

  const failures: [string, () => Promise<Response>, number, string][] = [
    [
      "an ErrorResponse",
      async () =>
        new Response(JSON.stringify({ error: "not found", code: "not_found" }), { status: 404 }),
      404,
      "not found",
    ],
    [
      "a body that is not an ErrorResponse",
      async () => new Response("<h1>Bad Gateway</h1>", { status: 502 }),
      502,
      "alchemyd answered with HTTP 502",
    ],
    [
      "an empty error field",
      async () => new Response(JSON.stringify({ error: "" }), { status: 500 }),
      500,
      "alchemyd answered with HTTP 500",
    ],
    [
      "a success body that is not JSON",
      async () => new Response("not json", { status: 200 }),
      200,
      "alchemyd sent a response Alchemy can't read",
    ],
    [
      "no response",
      async () => {
        throw new TypeError("Failed to fetch");
      },
      NO_RESPONSE,
      "alchemyd is not reachable",
    ],
  ];

  it.each(failures)("rejects with an ApiError for %s", async (_name, respond, status, message) => {
    vi.stubGlobal("fetch", respond);
    const err = await rejection(request("GET", "/engine"));
    expect(err).toBeInstanceOf(ApiError);
    expect(err).toMatchObject({ status, message });
  });

  it("passes an abort through unchanged", async () => {
    const abort = new DOMException("aborted", "AbortError");
    vi.stubGlobal("fetch", async () => {
      throw abort;
    });
    expect(await rejection(request("GET", "/engine"))).toBe(abort);
  });

  it("keeps the daemon's error code and tells a sign-in refusal apart", async () => {
    const answer = (status: number, body: unknown) => async () =>
      new Response(JSON.stringify(body), { status });
    vi.stubGlobal(
      "fetch",
      answer(401, { error: "not signed in: run `alchemy open`", code: "unauthorized" }),
    );
    const signedOut = await rejection(request("GET", "/apps"));
    expect(signedOut).toMatchObject({ status: 401, code: "unauthorized" });
    expect(isSignedOut(signedOut)).toBe(true);

    vi.stubGlobal("fetch", answer(403, { error: "cross-site request refused", code: "forbidden" }));
    const forbidden = await rejection(request("GET", "/apps"));
    expect(forbidden).toMatchObject({ status: 403, code: "forbidden" });
    expect(isSignedOut(forbidden)).toBe(false);

    vi.stubGlobal("fetch", answer(401, { error: "who are you" }));
    const noCode = await rejection(request("GET", "/apps"));
    expect(noCode).toMatchObject({ status: 401, code: null });
    expect(isSignedOut(noCode)).toBe(false);
    expect(isSignedOut(new Error("x"))).toBe(false);
  });

  it("sends no credential from a browser, which has its cookie", async () => {
    const fetch = vi.fn(async (_url: string, _init: RequestInit) => new Response("{}"));
    vi.stubGlobal("fetch", fetch);
    await request("POST", "/apps", { body: {} });
    expect(fetch.mock.calls[0]?.[1].headers).toEqual({ "content-type": "application/json" });
    expect(tauri.invoke).not.toHaveBeenCalled();
  });
});

describe("request in the desktop shell", () => {
  beforeEach(() => {
    (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
    forgetDaemonToken();
  });

  afterEach(() => {
    delete (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__;
    forgetDaemonToken();
    tauri.invoke.mockReset();
  });

  it("sends the daemon's token as a bearer token, read once", async () => {
    tauri.invoke.mockResolvedValue("a".repeat(64));
    const fetch = vi.fn(async (_url: string, _init: RequestInit) => new Response("{}"));
    vi.stubGlobal("fetch", fetch);
    await request("GET", "/apps");
    await request("PUT", "/settings/apps", { body: {} });
    expect(fetch.mock.calls.map(([, init]) => init.headers)).toEqual([
      { authorization: `Bearer ${"a".repeat(64)}` },
      { authorization: `Bearer ${"a".repeat(64)}`, "content-type": "application/json" },
    ]);
    expect(tauri.invoke).toHaveBeenCalledTimes(1);
    expect(tauri.invoke).toHaveBeenCalledWith("daemon_token");
  });

  it("reads the token again after a 401 and retries once", async () => {
    tauri.invoke.mockResolvedValueOnce("old").mockResolvedValue("new");
    const unauthorized = () =>
      new Response(JSON.stringify({ error: "not signed in", code: "unauthorized" }), {
        status: 401,
      });
    const fetch = vi.fn(async (_url: string, init: RequestInit) =>
      (init.headers as Record<string, string>).authorization === "Bearer new"
        ? new Response(JSON.stringify(ready))
        : unauthorized(),
    );
    vi.stubGlobal("fetch", fetch);
    await expect(api.engine()).resolves.toEqual(ready);
    expect(fetch).toHaveBeenCalledTimes(2);

    tauri.invoke.mockReset();
    tauri.invoke.mockResolvedValue("still wrong");
    forgetDaemonToken();
    const err = await rejection(api.engine());
    expect(isSignedOut(err)).toBe(true);
    expect(fetch).toHaveBeenCalledTimes(4);
  });

  it("sends no credential when the token can't be read, and tries again next time", async () => {
    tauri.invoke.mockRejectedValueOnce("no token file").mockResolvedValue("b".repeat(64));
    const fetch = vi.fn(async (_url: string, _init: RequestInit) => new Response("{}"));
    vi.stubGlobal("fetch", fetch);
    await request("GET", "/apps");
    await request("GET", "/apps");
    expect(fetch.mock.calls.map(([, init]) => init.headers)).toEqual([
      {},
      { authorization: `Bearer ${"b".repeat(64)}` },
    ]);
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

  it("reopens the stream when the browser stops retrying", () => {
    vi.useFakeTimers();
    try {
      const onEvent = vi.fn();
      const unsubscribe = subscribe(onEvent);
      const first = lastSource();
      first.readyState = 2;
      first.dispatchEvent(new Event("error"));
      vi.advanceTimersByTime(RESUBSCRIBE_DELAY_MS);
      expect(FakeEventSource.instances).toHaveLength(2);

      const second = lastSource();
      second.emit("engine", JSON.stringify(ready));
      expect(onEvent).toHaveBeenCalledWith({ event: "engine", data: ready });
      unsubscribe();
      expect(second.closed).toBe(true);
    } finally {
      vi.useRealTimers();
    }
  });

  it("leaves reconnecting to the browser while it is retrying", () => {
    vi.useFakeTimers();
    try {
      subscribe(() => {});
      lastSource().dispatchEvent(new Event("error"));
      vi.advanceTimersByTime(RESUBSCRIBE_DELAY_MS);
      expect(FakeEventSource.instances).toHaveLength(1);
    } finally {
      vi.useRealTimers();
    }
  });

  it("does not reopen after unsubscribing", () => {
    vi.useFakeTimers();
    try {
      const unsubscribe = subscribe(() => {});
      const source = lastSource();
      source.readyState = 2;
      source.dispatchEvent(new Event("error"));
      unsubscribe();
      vi.advanceTimersByTime(RESUBSCRIBE_DELAY_MS);
      expect(FakeEventSource.instances).toHaveLength(1);
    } finally {
      vi.useRealTimers();
    }
  });
});

describe("followOperation", () => {
  beforeEach(() => {
    FakeEventSource.instances = [];
    vi.stubGlobal("EventSource", FakeEventSource);
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("delivers output and closes after done", () => {
    const events: OperationEvent[] = [];
    followOperation("op 1", (event) => events.push(event));
    const source = FakeEventSource.last();
    expect(source.url).toBe("/api/operations/op%201/events");
    source.emit("output", JSON.stringify({ seq: 0, step: 0, text: "hi" }));
    source.emit("output", "not json");
    expect(source.closed).toBe(false);
    source.emit("done", JSON.stringify({ id: "op 1" }));
    expect(events.map((e) => e.event)).toEqual(["output", "done"]);
    expect(source.closed).toBe(true);
  });
});
