/** Minimal EventSource stand-in; jsdom does not implement one. */
export class FakeEventSource extends EventTarget {
  static instances: FakeEventSource[] = [];
  readonly url: string;
  closed = false;
  readyState = 0;

  constructor(url: string | URL) {
    super();
    this.url = String(url);
    FakeEventSource.instances.push(this);
  }

  /** The most recently opened stream. */
  static last(): FakeEventSource {
    const source = FakeEventSource.instances.at(-1);
    if (!source) throw new Error("no EventSource opened");
    return source;
  }

  /** Deliver one SSE frame, as the browser would. */
  emit(event: string, data: string): void {
    this.dispatchEvent(new MessageEvent(event, { data }));
  }

  close(): void {
    this.closed = true;
    this.readyState = 2;
  }
}
