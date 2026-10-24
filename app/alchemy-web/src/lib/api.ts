import { apiBase } from "./platform.ts";

/** Mirrors `alchemy_api::HealthResponse`. */
export interface HealthResponse {
  status: "ok";
}

/** Mirrors `alchemy_api::VersionResponse`. */
export interface VersionResponse {
  version: string;
  /** Mirrors `alchemy_api::API_VERSION`, bumped only on a breaking change. */
  api: number;
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
 * Mirrors `alchemy_api::MachineUsage`. CPU is a percentage of the whole
 * machine, rates are bytes per second, and sizes are bytes. `null` means not
 * measured, never zero. On macOS this is the Mac, not Docker Desktop's VM.
 */
export interface MachineUsage {
  cpu: number | null;
  /** The highest `cpu` within a rolled-up point. */
  cpu_max: number | null;
  cpus: number;
  /** The 1, 5, and 15 minute load averages, where the OS has them. */
  load: [number, number, number] | null;
  memory_used: number;
  memory_total: number;
  swap_used: number;
  swap_total: number;
  disk_used: number | null;
  disk_total: number | null;
  disk_read: number | null;
  disk_write: number | null;
  net_rx: number | null;
  net_tx: number | null;
}

/** Mirrors `alchemy_api::ContainerUsage`. CPU is a share of the whole engine host. */
export interface ContainerUsage {
  id: string;
  name: string;
  project: string | null;
  service: string | null;
  cpu: number | null;
  cpu_max: number | null;
  memory: number;
  memory_max: number | null;
  net_rx: number | null;
  net_tx: number | null;
  block_read: number | null;
  block_write: number | null;
}

/** Mirrors `alchemy_api::ProjectUsage`: one Compose project (an app) added up. */
export interface ProjectUsage {
  name: string;
  containers: number;
  cpu: number | null;
  memory: number;
  net_rx: number | null;
  net_tx: number | null;
}

/**
 * Mirrors `alchemy_api::MetricsResponse`, returned by `GET /api/metrics` and
 * sent as the `metrics` event after every sample.
 */
export interface MetricsResponse {
  /** Unix seconds of the sample, or `null` before the first. */
  at: number | null;
  /** Seconds between samples. */
  interval: number;
  /** Containers run in a VM (Docker Desktop, macOS): machine figures are the host, not the VM. */
  containers_in_vm: boolean;
  machine: MachineUsage | null;
  projects: ProjectUsage[];
  containers: ContainerUsage[];
}

/** Mirrors `alchemy_api::MetricsRange`. */
export type MetricsRange = "1h" | "12h" | "24h" | "7d" | "30d";

/** Mirrors `alchemy_api::MetricsPoint`. `t` is Unix seconds. */
export interface MetricsPoint {
  t: number;
  machine: MachineUsage | null;
  projects: ProjectUsage[];
  containers: ContainerUsage[];
}

/**
 * Mirrors `alchemy_api::MetricsHistory`, returned by
 * `GET /api/metrics/history`. Points more than one `step` apart have a gap
 * between them, which charts draw as a gap.
 */
export interface MetricsHistory {
  range: MetricsRange;
  step: number;
  project: string | null;
  points: MetricsPoint[];
}

/**
 * Mirrors `alchemy_api::ApiEvent`: one message on `GET /api/events`. The SSE
 * `event:` field is `event`, and its `data:` field is `data` as JSON.
 */
export type ApiEvent =
  | { event: "engine"; data: EngineStatus }
  | { event: "metrics"; data: MetricsResponse };

/** Every SSE event name the daemon sends. */
export type ApiEventName = ApiEvent["event"];

/** The payload of the event named `K`. */
export type ApiEventData<K extends ApiEventName> = Extract<ApiEvent, { event: K }>["data"];

/** One key per event name, so adding an {@link ApiEvent} variant fails to compile until listed. */
const EVENT_NAMES: { [K in ApiEventName]: K } = { engine: "engine", metrics: "metrics" };

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

/**
 * Mirrors `alchemy_api::ErrorCode`. A newer daemon may send a code not listed
 * here; the Rust client reads those as `unknown`.
 */
export type ErrorCode =
  | "not_found"
  | "method_not_allowed"
  | "bad_request"
  | "forbidden"
  | "unsupported_media_type"
  | "conflict"
  | "invalid"
  | "internal"
  | "unknown";

/** Mirrors `alchemy_api::ErrorResponse`, the body of every `/api` error. */
export interface ErrorResponse {
  /** Human-readable detail, safe to show to the user. */
  error: string;
  /** Machine-readable failure category. */
  code: ErrorCode;
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
 * Every method but `GET` declares `content-type: application/json`, body or
 * not, because alchemyd refuses a mutating request without it (415).
 * Rejects with an {@link ApiError} for every failure except an abort, which
 * rejects with the `AbortError` unchanged.
 */
export async function request<T>(
  method: Method,
  path: string,
  { body, signal }: RequestOptions = {},
): Promise<T> {
  const init: RequestInit = { method, signal };
  if (method !== "GET") {
    init.headers = { "content-type": "application/json" };
  }
  if (body !== undefined) {
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
  metrics: (signal?: AbortSignal) => request<MetricsResponse>("GET", "/metrics", { signal }),
  metricsHistory: (range: MetricsRange, project?: string, signal?: AbortSignal) => {
    const query = new URLSearchParams({ range });
    if (project !== undefined) query.set("project", project);
    return request<MetricsHistory>("GET", `/metrics/history?${query.toString()}`, { signal });
  },
  subscribe,
};
