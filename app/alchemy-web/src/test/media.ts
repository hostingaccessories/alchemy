import { vi } from "vite-plus/test";

/**
 * Stub `window.matchMedia` (jsdom lacks it) with an OS that prefers dark when
 * `dark` is true. Returns a setter that flips the preference and fires
 * `change` on every query, as a real OS switch would. Callers unstub globals
 * afterwards.
 */
export function stubSystemTheme(dark: boolean): (dark: boolean) => void {
  let prefersDark = dark;
  const queries: (MediaQueryList & { fire(): void })[] = [];

  vi.stubGlobal("matchMedia", (media: string) => {
    const target = new EventTarget();
    const query = Object.assign(target, {
      media,
      onchange: null,
      get matches() {
        return media === "(prefers-color-scheme: dark)" && prefersDark;
      },
      addListener: () => {},
      removeListener: () => {},
      fire: () => target.dispatchEvent(new Event("change")),
    }) as unknown as MediaQueryList & { fire(): void };
    queries.push(query);
    return query;
  });

  return (next) => {
    prefersDark = next;
    for (const query of queries) {
      query.fire();
    }
  };
}
