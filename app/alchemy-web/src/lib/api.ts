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
 * Open `GET /api/events` and call `onEvent` for every event. The first
 * `engine` event carries the current status, so no separate fetch is needed.
 * Frames whose data is not valid JSON are skipped.
 *
 * Returns a function that closes the stream.
 */
export function subscribe(
  onEvent: (event: ApiEvent) => void,
  options: SubscribeOptions = {},
): () => void {
  const source = new EventSource(`${apiBase()}/api/events`);
  for (const name of API_EVENT_NAMES) {
    source.addEventListener(name, (message: MessageEvent<string>) => {
      let data: unknown;
      try {
        data = JSON.parse(message.data);
      } catch {
        return;
      }
      onEvent({ event: name, data } as ApiEvent);
    });
  }
  if (options.onOpen) {
    source.addEventListener("open", options.onOpen);
  }
  if (options.onError) {
    source.addEventListener("error", options.onError);
  }
  return () => source.close();
}

export const api = {
  health: (signal?: AbortSignal) => getJson<HealthResponse>("/health", signal),
  version: (signal?: AbortSignal) => getJson<VersionResponse>("/version", signal),
  engine: (signal?: AbortSignal) => getJson<EngineStatus>("/engine", signal),
  subscribe,
};
