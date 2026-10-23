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

/**
 * `url` parsed, if Alchemy may open it outside the app; otherwise `null`.
 * Only `https:` passes, matching the desktop shell's opener scope.
 */
export function externalUrl(url: string): URL | null {
  try {
    const parsed = new URL(url);
    return parsed.protocol === "https:" ? parsed : null;
  } catch {
    return null;
  }
}

/**
 * Open an `https:` URL in the user's default browser. The desktop shell
 * hands it to the opener plugin, since its webview opens no windows; a
 * browser opens a new tab with no `window.opener` and no referrer. Rejects
 * any other URL without opening anything.
 */
export async function openExternal(url: string): Promise<void> {
  const parsed = externalUrl(url);
  if (!parsed) {
    throw new Error("Only https: links open outside Alchemy");
  }
  if (isTauri()) {
    const { openUrl } = await import("@tauri-apps/plugin-opener");
    await openUrl(parsed.href);
    return;
  }
  window.open(parsed.href, "_blank", "noopener,noreferrer");
}
