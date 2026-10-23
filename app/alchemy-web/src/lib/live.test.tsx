import { QueryClient, QueryClientProvider, useQuery } from "@tanstack/react-query";
import { act, cleanup, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vite-plus/test";

import { FakeEventSource } from "../test/event-source.ts";
import { API_EVENT_NAMES, type EngineStatus, request } from "./api.ts";
import { LiveProvider, useDaemonConnection } from "./live.tsx";
import { liveQueries } from "./queries.ts";

const ready: EngineStatus = {
  state: "ready",
  flavor: "docker_desktop",
  engine_version: "29.8.1",
  api_version: "1.52",
  compose_version: "2.40.0",
  desktop_version: "4.93.0",
  endpoint: "unix:///var/run/docker.sock",
};

/** Whether the stubbed `GET /api/version` answers. */
let daemonUp = true;
let fetch: ReturnType<typeof vi.fn<(url: string) => Promise<Response>>>;

function fetched(path: string): number {
  return fetch.mock.calls.filter(([url]) => url === `/api${path}`).length;
}

/** Reads one live query, the daemon connection, and one query the stream does not feed. */
function Page() {
  const daemon = useDaemonConnection();
  const engine = useQuery(liveQueries.engine);
  const other = useQuery({ queryKey: ["other"], queryFn: () => request<number>("GET", "/other") });
  return (
    <>
      <p data-testid="daemon">
        {daemon.kind === "online"
          ? `online ${daemon.version}`
          : daemon.kind === "offline"
            ? `offline ${daemon.error.status} ${daemon.error.message}`
            : "connecting"}
      </p>
      <p data-testid="engine">{engine.data?.state ?? "none"}</p>
      <p data-testid="other">{other.data ?? "none"}</p>
    </>
  );
}

function renderLive() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const result = render(
    <QueryClientProvider client={client}>
      <LiveProvider>
        <Page />
      </LiveProvider>
    </QueryClientProvider>,
  );
  return { ...result, client };
}

function text(id: string): string {
  return screen.getByTestId(id).textContent ?? "";
}

function emit(event: string, data: unknown) {
  act(() => FakeEventSource.last().emit(event, JSON.stringify(data)));
}

function dispatch(type: "open" | "error") {
  act(() => {
    FakeEventSource.last().dispatchEvent(new Event(type));
  });
}

beforeEach(() => {
  daemonUp = true;
  FakeEventSource.instances = [];
  vi.stubGlobal("EventSource", FakeEventSource);
  fetch = vi.fn(async (url: string) => {
    if (url === "/api/version") {
      if (!daemonUp) throw new TypeError("connection refused");
      return new Response(JSON.stringify({ version: "9.9.9" }));
    }
    if (url === "/api/other") {
      return new Response(JSON.stringify(fetched("/other")));
    }
    return new Promise<Response>(() => {});
  });
  vi.stubGlobal("fetch", fetch);
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("liveQueries", () => {
  it("has one entry per event name", () => {
    expect(Object.keys(liveQueries).sort()).toEqual([...API_EVENT_NAMES].sort());
  });
});

describe("LiveProvider", () => {
  it("writes each event into its query", async () => {
    renderLive();
    expect(text("engine")).toBe("none");

    emit("engine", ready);
    expect(await screen.findByText("ready")).toBeTruthy();

    emit("engine", { ...ready, state: "not_running" });
    expect(await screen.findByText("not_running")).toBeTruthy();
  });

  it("ignores events it doesn't know", async () => {
    const { client } = renderLive();
    emit("stacks", []);
    emit("engine", ready);
    expect(await screen.findByText("ready")).toBeTruthy();
    expect(client.getQueryData(["stacks"])).toBeUndefined();
  });

  it("opens one stream and closes it on unmount", () => {
    const { unmount } = renderLive();
    expect(FakeEventSource.instances).toHaveLength(1);
    expect(FakeEventSource.last().url).toBe("/api/events");
    unmount();
    expect(FakeEventSource.last().closed).toBe(true);
  });

  it("refetches nothing on the first open", async () => {
    renderLive();
    await screen.findByText("online 9.9.9");
    await screen.findByText("1");
    dispatch("open");
    await act(async () => {});
    expect(fetched("/version")).toBe(1);
    expect(fetched("/other")).toBe(1);
  });

  it("invalidates the cache on reconnect, refetching only what the stream does not resend", async () => {
    const { client } = renderLive();
    dispatch("open");
    emit("engine", ready);
    await screen.findByText("1");
    expect(fetched("/engine")).toBe(1);

    dispatch("error");
    dispatch("open");
    expect(await screen.findByText("2")).toBeTruthy();
    expect(fetched("/engine")).toBe(1);
    expect(client.getQueryState(liveQueries.engine.queryKey)?.isInvalidated).toBe(true);

    emit("engine", { ...ready, state: "starting" });
    expect(await screen.findByText("starting")).toBeTruthy();
    expect(client.getQueryState(liveQueries.engine.queryKey)?.isInvalidated).toBe(false);
  });
});

describe("useDaemonConnection", () => {
  it("is connecting, then online with the daemon's version", async () => {
    renderLive();
    expect(text("daemon")).toBe("connecting");
    expect(await screen.findByText("online 9.9.9")).toBeTruthy();
  });

  it("is offline with an ApiError when the daemon doesn't answer", async () => {
    daemonUp = false;
    renderLive();
    expect(await screen.findByText("offline 0 alchemyd is not reachable")).toBeTruthy();
  });

  it("re-probes when the stream drops, and recovers when it reconnects", async () => {
    renderLive();
    await screen.findByText("online 9.9.9");

    daemonUp = false;
    dispatch("error");
    expect(await screen.findByText("offline 0 alchemyd is not reachable")).toBeTruthy();

    daemonUp = true;
    dispatch("open");
    expect(await screen.findByText("online 9.9.9")).toBeTruthy();
  });

  it("stays online while the stream reconnects to a daemon that still answers", async () => {
    renderLive();
    await screen.findByText("online 9.9.9");
    dispatch("error");
    await act(async () => {});
    expect(fetched("/version")).toBe(2);
    expect(text("daemon")).toBe("online 9.9.9");
  });
});
