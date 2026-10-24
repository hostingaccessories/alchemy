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

let token: Promise<string | null> | null = null;

/**
 * alchemyd's TCP token, which the desktop shell reads from the file alchemyd
 * writes at each start (the `daemon_token` command). The shell's UI is served
 * from its own origin, so the browser's sign-in cookie never reaches it and
 * every request carries the token instead. `null` in a browser, or when the
 * shell can't read it. Kept until {@link forgetDaemonToken}; a failed read is
 * not kept, so the next call tries again.
 */
export function daemonToken(): Promise<string | null> {
  if (!isTauri()) {
    return Promise.resolve(null);
  }
  if (token === null) {
    const read = import("@tauri-apps/api/core")
      .then(({ invoke }) => invoke<string>("daemon_token"))
      .catch(() => {
        if (token === read) token = null;
        return null;
      });
    token = read;
  }
  return token;
}

/** Drop the kept token, after a 401 says alchemyd has restarted with a new one. */
export function forgetDaemonToken(): void {
  token = null;
}

/**
 * Headers that sign a request to alchemyd's API in: `Authorization: Bearer`
 * in the desktop shell, nothing in a browser, which sends its cookie.
 */
export async function authHeaders(): Promise<Record<string, string>> {
  const value = await daemonToken();
  return value === null ? {} : { authorization: `Bearer ${value}` };
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

/**
 * Set the desktop window's native title bar to `theme`, or back to the OS
 * theme with `null`. Does nothing in a browser. The Tauri code loads only
 * in the shell, so the browser bundle doesn't carry it.
 */
export async function setWindowTheme(theme: "light" | "dark" | null): Promise<void> {
  if (!isTauri()) {
    return;
  }
  const { getCurrentWindow } = await import("@tauri-apps/api/window");
  await getCurrentWindow().setTheme(theme);
}
