import type { LogEvent } from "./api.ts";
import { apiBase } from "./platform.ts";

/** What to read of an app's logs. */
export interface LogsQuery {
  /** Only this service's lines, or every service's when absent. */
  service?: string;
  /** How many of the last lines to start with; the daemon defaults to 200. */
  tail?: number;
  /** Keep sending new lines as they arrive. */
  follow: boolean;
}

/** Callbacks for {@link openLogs}. */
export interface LogsHandlers {
  /** The stream (re)connected. The daemon resends the tail, so drop any lines shown. */
  onOpen: () => void;
  /** One event, in the order the daemon sent it. */
  onEvent: (event: LogEvent) => void;
  /**
   * The connection dropped before `done`. `retrying` says whether the
   * browser is reconnecting on its own; when it isn't, the stream is closed.
   */
  onError: (retrying: boolean) => void;
}

/** The logs route for `app` and `query`, under `/api`. */
export function logsPath(app: string, query: LogsQuery): string {
  const params = new URLSearchParams();
  if (query.service !== undefined) params.set("service", query.service);
  if (query.tail !== undefined) params.set("tail", String(query.tail));
  if (query.follow) params.set("follow", "true");
  const search = params.toString();
  return `/apps/${encodeURIComponent(app)}/logs${search === "" ? "" : `?${search}`}`;
}

const EVENT_NAMES: readonly LogEvent["event"][] = ["log", "following", "done"];

/** `EventSource.CLOSED`. */
const CLOSED = 2;

/**
 * Open `GET /api/apps/{app}/logs` and pass each event to `handlers`. The
 * stream is closed after `done`, so the browser doesn't reconnect to a
 * finished stream. Frames whose data is not valid JSON are skipped.
 *
 * Returns a function that closes the stream.
 */
export function openLogs(app: string, query: LogsQuery, handlers: LogsHandlers): () => void {
  const source = new EventSource(`${apiBase()}/api${logsPath(app, query)}`);
  for (const name of EVENT_NAMES) {
    source.addEventListener(name, (message: MessageEvent<string>) => {
      let data: unknown;
      try {
        data = JSON.parse(message.data);
      } catch {
        return;
      }
      if (name === "done") {
        source.close();
      }
      handlers.onEvent({ event: name, data } as LogEvent);
    });
  }
  source.addEventListener("open", () => handlers.onOpen());
  source.addEventListener("error", () => handlers.onError(source.readyState !== CLOSED));
  return () => source.close();
}
