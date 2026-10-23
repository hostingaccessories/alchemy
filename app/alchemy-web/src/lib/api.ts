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

/** The payload of the event named `K`. */
export type ApiEventData<K extends ApiEventName> = Extract<ApiEvent, { event: K }>["data"];

/** One key per event name, so adding an {@link ApiEvent} variant fails to compile until listed. */
const EVENT_NAMES: { [K in ApiEventName]: K } = { engine: "engine" };

/** The event names {@link subscribe} listens for. Others are ignored. */
export const API_EVENT_NAMES: readonly ApiEventName[] = Object.values(EVENT_NAMES);

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

/** Mirrors `alchemy_api::ErrorResponse`, the body of every `/api` error. */
export interface ErrorResponse {
  /** Human-readable detail, safe to show to the user. */
  error: string;
}

/** An HTTP method {@link request} sends. */
export type Method = "GET" | "POST" | "PUT" | "DELETE";

/** `ApiError.status` when no response arrived at all. */
export const NO_RESPONSE = 0;

/**
 * Why an `/api` request failed. `message` is always safe to show the user:
 * the daemon's {@link ErrorResponse} text when it sent one, or a generic line
 * otherwise. `status` is the HTTP status, or {@link NO_RESPONSE} when the
 * daemon could not be reached.
 */
export class ApiError extends Error {
  readonly status: number;

  constructor(status: number, message: string) {
    super(message);
    this.name = "ApiError";
    this.status = status;
  }
}

/** Options for {@link request}. */
export interface RequestOptions {
  /** Sent as JSON. */
  body?: unknown;
  signal?: AbortSignal;
}

function isAbort(err: unknown): boolean {
  return err instanceof DOMException && err.name === "AbortError";
}

/** The daemon's {@link ErrorResponse} text, or a generic line for any other body. */
async function errorMessage(res: Response): Promise<string> {
  let body: unknown;
  try {
    body = await res.json();
  } catch {
    body = null;
  }
  const error = (body as Partial<ErrorResponse> | null)?.error;
  return typeof error === "string" && error !== ""
    ? error
    : `alchemyd answered with HTTP ${res.status}`;
}

/**
 * Send one request to `/api{path}` and return the JSON response, or
 * `undefined` for `204 No Content`. `body`, when given, is sent as JSON.
 * Rejects with an {@link ApiError} for every failure except an abort, which
 * rejects with the `AbortError` unchanged.
 */
export async function request<T>(
  method: Method,
  path: string,
  { body, signal }: RequestOptions = {},
): Promise<T> {
  const init: RequestInit = { method, signal };
  if (body !== undefined) {
    init.headers = { "content-type": "application/json" };
    init.body = JSON.stringify(body);
  }

  let res: Response;
  try {
    res = await fetch(`${apiBase()}/api${path}`, init);
  } catch (err) {
    if (isAbort(err)) throw err;
    throw new ApiError(NO_RESPONSE, "alchemyd is not reachable");
  }

  if (!res.ok) {
    throw new ApiError(res.status, await errorMessage(res));
  }
  if (res.status === 204) {
    return undefined as T;
  }
  try {
    return (await res.json()) as T;
  } catch (err) {
    if (isAbort(err)) throw err;
    throw new ApiError(res.status, "alchemyd sent a response Alchemy can't read");
  }
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
  health: (signal?: AbortSignal) => request<HealthResponse>("GET", "/health", { signal }),
  version: (signal?: AbortSignal) => request<VersionResponse>("GET", "/version", { signal }),
  engine: (signal?: AbortSignal) => request<EngineStatus>("GET", "/engine", { signal }),
  subscribe,
};
