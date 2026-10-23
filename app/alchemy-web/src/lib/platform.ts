/** Default daemon address used by the desktop shell. Mirrors `alchemyd::DEFAULT_LISTEN`. */
export const DEFAULT_DAEMON_URL = "http://127.0.0.1:7240";

/** True when running inside the Tauri desktop shell rather than a browser. */
export function isTauri(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

/**
 * Base URL for API calls. In a browser the UI is served by alchemyd (or proxied
 * by the dev server), so requests are same-origin. The desktop shell loads the
 * UI from its own bundle and must address the daemon explicitly.
 */
export function apiBase(): string {
  return isTauri() && !import.meta.env.DEV ? DEFAULT_DAEMON_URL : "";
}
