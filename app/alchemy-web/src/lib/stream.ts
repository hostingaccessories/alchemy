import { authHeaders, forgetDaemonToken, isTauri } from "./platform.ts";

/** `EventSource.CONNECTING`. */
export const CONNECTING = 0;
/** `EventSource.OPEN`. */
export const OPEN = 1;
/** `EventSource.CLOSED`: nothing will reconnect it. */
export const CLOSED = 2;

/**
 * How long {@link FetchEventSource} waits before reconnecting a stream that
 * dropped, like a browser's `EventSource`.
 */
export const RECONNECT_DELAY_MS = 2000;

/** The parts of `EventSource` Alchemy uses. */
export interface EventStream extends EventTarget {
  readonly readyState: number;
  close(): void;
}

/**
 * Open the server-sent event stream at `url`. A browser uses `EventSource`,
 * whose requests carry the sign-in cookie. The desktop shell can't: its UI is
 * served from another origin, so it has no cookie, and `EventSource` can't
 * send an `Authorization` header. There it is a {@link FetchEventSource},
 * which can.
 */
export function openEventStream(url: string): EventStream {
  return isTauri() ? new FetchEventSource(url) : new EventSource(url);
}

/** One event of a server-sent event stream. */
export interface SseMessage {
  event: string;
  data: string;
}

/**
 * Turns the text of a server-sent event stream, in chunks as it arrives,
 * into events, as the HTML standard's parser does. `id` and `retry` are
 * ignored; Alchemy's streams send neither.
 */
export class SseParser {
  #buffer = "";
  #event = "";
  #data: string[] = [];

  /** Feed `chunk` and return every event it completed. */
  push(chunk: string): SseMessage[] {
    this.#buffer += chunk;
    const done: SseMessage[] = [];
    for (;;) {
      const end = this.#buffer.search(/\r\n|\r|\n/);
      if (end === -1) break;
      if (this.#buffer[end] === "\r" && end === this.#buffer.length - 1) break;
      const line = this.#buffer.slice(0, end);
      const width = this.#buffer.startsWith("\r\n", end) ? 2 : 1;
      this.#buffer = this.#buffer.slice(end + width);
      const message = this.#line(line);
      if (message) done.push(message);
    }
    return done;
  }

  #line(line: string): SseMessage | null {
    if (line === "") {
      const message =
        this.#data.length > 0
          ? { event: this.#event === "" ? "message" : this.#event, data: this.#data.join("\n") }
          : null;
      this.#event = "";
      this.#data = [];
      return message;
    }
    if (line.startsWith(":")) return null;
    const colon = line.indexOf(":");
    const field = colon === -1 ? line : line.slice(0, colon);
    let value = colon === -1 ? "" : line.slice(colon + 1);
    if (value.startsWith(" ")) value = value.slice(1);
    if (field === "event") this.#event = value;
    if (field === "data") this.#data.push(value);
    return null;
  }
}

/**
 * An `EventSource` that reads the stream with `fetch`, so it can send the
 * desktop shell's token. It dispatches the same `open`, `error`, and named
 * message events, and reconnects the same way: after a dropped connection
 * or a stream that ended, but not after an error status, which closes it. A
 * 401 also drops the kept token, so the next attempt reads alchemyd's new
 * one.
 */
export class FetchEventSource extends EventTarget implements EventStream {
  readonly url: string;
  #state = CONNECTING;
  #abort = new AbortController();
  #retry: ReturnType<typeof setTimeout> | undefined;

  constructor(url: string) {
    super();
    this.url = url;
    void this.#connect();
  }

  get readyState(): number {
    return this.#state;
  }

  close(): void {
    this.#state = CLOSED;
    clearTimeout(this.#retry);
    this.#abort.abort();
  }

  async #connect(): Promise<void> {
    this.#retry = undefined;
    const { signal } = this.#abort;
    let res: Response;
    try {
      res = await fetch(this.url, {
        headers: { accept: "text/event-stream", ...(await authHeaders()) },
        cache: "no-store",
        signal,
      });
    } catch {
      this.#dropped();
      return;
    }
    if (this.#state === CLOSED) return;
    const type = res.headers.get("content-type") ?? "";
    if (!res.ok || res.body === null || !type.startsWith("text/event-stream")) {
      if (res.status === 401) forgetDaemonToken();
      this.#state = CLOSED;
      this.dispatchEvent(new Event("error"));
      return;
    }
    this.#state = OPEN;
    this.dispatchEvent(new Event("open"));
    const reader = res.body.getReader();
    const decoder = new TextDecoder();
    const parser = new SseParser();
    try {
      for (;;) {
        const { done, value } = await reader.read();
        if (done || this.#state === CLOSED) break;
        for (const message of parser.push(decoder.decode(value, { stream: true }))) {
          if (this.#state === CLOSED) break;
          this.dispatchEvent(new MessageEvent(message.event, { data: message.data }));
        }
      }
    } catch {
      // A dropped connection; reconnect below.
    }
    this.#dropped();
  }

  #dropped(): void {
    if (this.#state === CLOSED) return;
    this.#state = CONNECTING;
    this.dispatchEvent(new Event("error"));
    if (this.#state === CONNECTING) {
      this.#retry = setTimeout(() => void this.#connect(), RECONNECT_DELAY_MS);
    }
  }
}
