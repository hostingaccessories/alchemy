import { afterEach, beforeEach, describe, expect, it, vi } from "vite-plus/test";

import { FakeEventSource } from "../test/event-source.ts";
import { forgetDaemonToken } from "./platform.ts";
import {
  CLOSED,
  CONNECTING,
  FetchEventSource,
  OPEN,
  RECONNECT_DELAY_MS,
  SseParser,
  openEventStream,
} from "./stream.ts";

const tauri = vi.hoisted(() => ({
  invoke: vi.fn(async (_command: string): Promise<string> => "t0ken"),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: tauri.invoke }));

function enterShell() {
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
}

afterEach(() => {
  delete (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__;
  forgetDaemonToken();
  vi.unstubAllGlobals();
  vi.useRealTimers();
  tauri.invoke.mockClear();
});

describe("SseParser", () => {
  it.each([
    ["one event", ["event: engine\ndata: {}\n\n"], [["engine", "{}"]]],
    ["no event name", ["data: hi\n\n"], [["message", "hi"]]],
    ["split across chunks", ["event: lo", "g\ndata: a", "b\n", "\n"], [["log", "ab"]]],
    ["multi-line data", ["data: a\ndata: b\n\n"], [["message", "a\nb"]]],
    ["CRLF", ["event: x\r\ndata: 1\r\n\r\n"], [["x", "1"]]],
    ["CR split from LF", ["event: x\r", "\ndata: 1\r\n\r\n"], [["x", "1"]]],
    ["lone CR", ["data: 1\r\r", ":"], [["message", "1"]]],
    ["CR at the end of a chunk waits for the next", ["data: 1\r\r"], []],
    ["comments and keep-alives", [":\n\n: ping\n\ndata: 1\n\n"], [["message", "1"]]],
    ["no space after the colon", ["event:x\ndata:1\n\n"], [["x", "1"]]],
    ["only the first space is dropped", ["data:  1\n\n"], [["message", " 1"]]],
    ["empty data", ["data\n\n"], [["message", ""]]],
    ["event without data", ["event: x\n\ndata: 1\n\n"], [["message", "1"]]],
    ["unknown fields", ["id: 7\nretry: 10\nfoo: bar\ndata: 1\n\n"], [["message", "1"]]],
    ["unfinished", ["data: 1\n"], []],
  ])("%s", (_name, chunks, want) => {
    const parser = new SseParser();
    const got = chunks.flatMap((chunk) => parser.push(chunk)).map((m) => [m.event, m.data]);
    expect(got).toEqual(want);
  });
});

/** A response streaming `chunks`, then ending, or staying open with `hold`. */
function streaming(chunks: string[], { hold = false, status = 200 } = {}): Response {
  const encoder = new TextEncoder();
  const body = new ReadableStream<Uint8Array>({
    start(controller) {
      for (const chunk of chunks) controller.enqueue(encoder.encode(chunk));
      if (!hold) controller.close();
    },
  });
  return new Response(body, { status, headers: { "content-type": "text/event-stream" } });
}

/** Collect what `source` dispatches, as `type` or `type:data`. */
function record(source: EventTarget, names: string[]): string[] {
  const seen: string[] = [];
  for (const name of ["open", "error", ...names]) {
    source.addEventListener(name, (event) => {
      seen.push(event instanceof MessageEvent ? `${name}:${String(event.data)}` : name);
    });
  }
  return seen;
}

describe("openEventStream", () => {
  beforeEach(() => {
    FakeEventSource.instances = [];
    vi.stubGlobal("EventSource", FakeEventSource);
  });

  it("is the browser's EventSource outside the shell", () => {
    const source = openEventStream("/api/events");
    expect(source).toBeInstanceOf(FakeEventSource);
  });

  it("reads with fetch and the token in the shell", async () => {
    enterShell();
    const fetch = vi.fn(async (_url: string, _init: RequestInit) => streaming([], { hold: true }));
    vi.stubGlobal("fetch", fetch);
    const source = openEventStream("http://127.0.0.1:7240/api/events");
    expect(source).toBeInstanceOf(FetchEventSource);
    await vi.waitFor(() => expect(source.readyState).toBe(OPEN));
    expect(fetch.mock.calls[0]?.[0]).toBe("http://127.0.0.1:7240/api/events");
    expect(fetch.mock.calls[0]?.[1].headers).toEqual({
      accept: "text/event-stream",
      authorization: "Bearer t0ken",
    });
    source.close();
    expect(FakeEventSource.instances).toEqual([]);
  });
});

describe("FetchEventSource", () => {
  beforeEach(() => {
    enterShell();
  });

  it("dispatches open and each named event", async () => {
    vi.stubGlobal("fetch", async () =>
      streaming(['event: engine\ndata: {"state":', '"ready"}\n\n: keep-alive\n\n'], {
        hold: true,
      }),
    );
    const source = new FetchEventSource("/api/events");
    const seen = record(source, ["engine"]);
    await vi.waitFor(() => expect(seen).toEqual(["open", 'engine:{"state":"ready"}']));
    source.close();
    expect(source.readyState).toBe(CLOSED);
  });

  it("reconnects after the stream ends, like EventSource", async () => {
    vi.useFakeTimers();
    const fetch = vi.fn(async () => streaming(["data: 1\n\n"]));
    vi.stubGlobal("fetch", fetch);
    const source = new FetchEventSource("/api/events");
    const seen = record(source, ["message"]);
    await vi.waitFor(() => expect(seen).toEqual(["open", "message:1", "error"]));
    expect(source.readyState).toBe(CONNECTING);
    expect(fetch).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(RECONNECT_DELAY_MS);
    await vi.waitFor(() => expect(fetch).toHaveBeenCalledTimes(2));
    source.close();
  });

  it("reconnects after a refused connection", async () => {
    vi.useFakeTimers();
    const fetch = vi.fn(async () => {
      throw new TypeError("Failed to fetch");
    });
    vi.stubGlobal("fetch", fetch);
    const source = new FetchEventSource("/api/events");
    const seen = record(source, []);
    await vi.waitFor(() => expect(seen).toEqual(["error"]));
    expect(source.readyState).toBe(CONNECTING);
    await vi.advanceTimersByTimeAsync(RECONNECT_DELAY_MS);
    expect(fetch).toHaveBeenCalledTimes(2);
    source.close();
    await vi.advanceTimersByTimeAsync(RECONNECT_DELAY_MS * 2);
    expect(fetch).toHaveBeenCalledTimes(2);
  });

  it("closes on a 401 and reads the token again next time", async () => {
    vi.useFakeTimers();
    const fetch = vi.fn(
      async () =>
        new Response(JSON.stringify({ error: "not signed in", code: "unauthorized" }), {
          status: 401,
          headers: { "content-type": "application/json" },
        }),
    );
    vi.stubGlobal("fetch", fetch);
    const first = new FetchEventSource("/api/events");
    const seen = record(first, []);
    await vi.waitFor(() => expect(seen).toEqual(["error"]));
    expect(first.readyState).toBe(CLOSED);
    await vi.advanceTimersByTimeAsync(RECONNECT_DELAY_MS * 2);
    expect(fetch).toHaveBeenCalledTimes(1);
    expect(tauri.invoke).toHaveBeenCalledTimes(1);

    const second = new FetchEventSource("/api/events");
    await vi.waitFor(() => expect(second.readyState).toBe(CLOSED));
    expect(tauri.invoke).toHaveBeenCalledTimes(2);
  });

  it("dispatches nothing after close", async () => {
    let push: ((chunk: string) => void) | undefined;
    vi.stubGlobal("fetch", async () => {
      const encoder = new TextEncoder();
      const body = new ReadableStream<Uint8Array>({
        start(controller) {
          push = (chunk) => controller.enqueue(encoder.encode(chunk));
        },
      });
      return new Response(body, { headers: { "content-type": "text/event-stream" } });
    });
    const source = new FetchEventSource("/api/events");
    const seen = record(source, ["message"]);
    await vi.waitFor(() => expect(seen).toEqual(["open"]));
    source.close();
    push?.("data: late\n\n");
    await new Promise((resolve) => setTimeout(resolve, 10));
    expect(seen).toEqual(["open"]);
  });
});
