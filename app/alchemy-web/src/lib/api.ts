import { apiBase } from "./platform.ts";

/** Mirrors `alchemy_api::HealthResponse`. */
export interface HealthResponse {
  status: "ok";
}

/** Mirrors `alchemy_api::VersionResponse`. */
export interface VersionResponse {
  version: string;
}

/** Mirrors `alchemy_api::EngineState`. */
export type EngineState = "not_installed" | "not_running" | "starting" | "ready" | "unsupported";

/** Mirrors `alchemy_api::EngineFlavor`. */
export type EngineFlavor = "docker_desktop" | "docker_engine" | "other";

/**
 * Mirrors `alchemy_api::EngineStatus`, returned by `GET /api/engine`. Fields
 * other than `state` are `null` until the engine has answered enough to know
 * them.
 */
export interface EngineStatus {
  state: EngineState;
  flavor: EngineFlavor | null;
  engine_version: string | null;
  api_version: string | null;
  compose_version: string | null;
  desktop_version: string | null;
  endpoint: string | null;
}

/**
 * Mirrors `alchemy_api::ApiEvent`: one message on `GET /api/events`. The SSE
 * `event:` field is `event`, and its `data:` field is `data` as JSON.
 */
export type ApiEvent = { event: "engine"; data: EngineStatus };

/** Every SSE event name the daemon sends. */
export type ApiEventName = ApiEvent["event"];

/** The event names {@link subscribe} listens for. Others are ignored. */
export const API_EVENT_NAMES: readonly ApiEventName[] = ["engine"];

/** Optional callbacks for {@link subscribe}. */
export interface SubscribeOptions {
  /** The stream connected, or reconnected after an error. */
  onOpen?: () => void;
  /**
   * The stream dropped or failed to connect. The browser retries on its own;
   * the daemon resends the current state when it does.
   */
  onError?: () => void;
}

async function getJson<T>(path: string, signal?: AbortSignal): Promise<T> {
  const res = await fetch(`${apiBase()}/api${path}`, { signal });
  if (!res.ok) {
    throw new Error(`GET /api${path} failed: ${res.status}`);
  }
  return (await res.json()) as T;
}

/**
 * How long {@link subscribe} waits before reopening a stream the browser has
 * stopped retrying, which it does when a reconnect gets an error response
 * (a dev proxy with the daemon down) instead of a refused connection.
 */
export const RESUBSCRIBE_DELAY_MS = 2000;

/** `EventSource.CLOSED`: the browser will not reconnect on its own. */
const EVENT_SOURCE_CLOSED = 2;

/**
 * Open `GET /api/events` and call `onEvent` for every event. The first
 * `engine` event carries the current status, so no separate fetch is needed.
 * Frames whose data is not valid JSON are skipped. If the browser gives up
 * reconnecting, the stream is reopened after {@link RESUBSCRIBE_DELAY_MS}.
 *
 * Returns a function that closes the stream.
 */
export function subscribe(
  onEvent: (event: ApiEvent) => void,
  options: SubscribeOptions = {},
): () => void {
  let closed = false;
  let retry: ReturnType<typeof setTimeout> | undefined;
  let source: EventSource;

  const open = () => {
    retry = undefined;
    const current = new EventSource(`${apiBase()}/api/events`);
    source = current;
    for (const name of API_EVENT_NAMES) {
      current.addEventListener(name, (message: MessageEvent<string>) => {
        let data: unknown;
        try {
          data = JSON.parse(message.data);
        } catch {
          return;
        }
        onEvent({ event: name, data } as ApiEvent);
      });
    }
    current.addEventListener("open", () => options.onOpen?.());
    current.addEventListener("error", () => {
      options.onError?.();
      if (!closed && retry === undefined && current.readyState === EVENT_SOURCE_CLOSED) {
        retry = setTimeout(open, RESUBSCRIBE_DELAY_MS);
      }
    });
  };

  open();
  return () => {
    closed = true;
    clearTimeout(retry);
    source.close();
  };
}

export const api = {
  health: (signal?: AbortSignal) => getJson<HealthResponse>("/health", signal),
  version: (signal?: AbortSignal) => getJson<VersionResponse>("/version", signal),
  engine: (signal?: AbortSignal) => getJson<EngineStatus>("/engine", signal),
  subscribe,
};
