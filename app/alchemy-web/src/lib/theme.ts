import { useSyncExternalStore } from "react";

/**
 * The `localStorage` key holding the theme preference. The pre-paint script
 * in `public/theme.js` reads the same key; change both together.
 */
export const THEME_STORAGE_KEY = "alchemy.theme";

/** What the user picked on the Settings page. */
export type ThemePreference = "system" | "light" | "dark";

/** The theme actually on screen. */
export type Theme = "light" | "dark";

/** The options in the order the Settings page shows them. */
export const THEME_PREFERENCES: readonly ThemePreference[] = ["system", "light", "dark"];

const DARK_QUERY = "(prefers-color-scheme: dark)";

/** A stored value, or `system` for anything missing or unknown. */
export function parseThemePreference(raw: string | null | undefined): ThemePreference {
  return raw === "light" || raw === "dark" ? raw : "system";
}

/** The theme a preference shows, given whether the OS prefers dark. */
export function resolveTheme(preference: ThemePreference, systemDark: boolean): Theme {
  if (preference === "system") {
    return systemDark ? "dark" : "light";
  }
  return preference;
}

/**
 * Set or clear `data-theme` on `root`. Without the attribute, `style.css`
 * follows `prefers-color-scheme`, so `system` needs no script to track the OS.
 */
export function applyThemePreference(root: HTMLElement, preference: ThemePreference): void {
  if (preference === "system") {
    delete root.dataset.theme;
  } else {
    root.dataset.theme = preference;
  }
}

/** A subscribable theme preference, persisted to `localStorage` when it can be. */
export interface ThemeStore {
  get: () => ThemePreference;
  set: (preference: ThemePreference) => void;
  subscribe: (listener: () => void) => () => void;
}

/**
 * Build a {@link ThemeStore}. Storage can be missing or throw (private
 * windows, blocked site data), so every access is guarded: when a write
 * fails, the choice still applies for the life of the page.
 */
export function createThemeStore(storage: () => Storage, root: HTMLElement): ThemeStore {
  let unsaved: ThemePreference | null = null;
  const listeners = new Set<() => void>();

  const read = (): ThemePreference => {
    if (unsaved) {
      return unsaved;
    }
    try {
      return parseThemePreference(storage().getItem(THEME_STORAGE_KEY));
    } catch {
      return "system";
    }
  };

  const notify = () => {
    for (const listener of listeners) {
      listener();
    }
  };

  const onStorage = (event: StorageEvent) => {
    if (event.key === THEME_STORAGE_KEY || event.key === null) {
      unsaved = null;
      applyThemePreference(root, read());
      notify();
    }
  };

  return {
    get: read,
    set: (preference) => {
      try {
        if (preference === "system") {
          storage().removeItem(THEME_STORAGE_KEY);
        } else {
          storage().setItem(THEME_STORAGE_KEY, preference);
        }
        unsaved = null;
      } catch {
        unsaved = preference;
      }
      applyThemePreference(root, preference);
      notify();
    },
    subscribe: (listener) => {
      if (listeners.size === 0) {
        window.addEventListener("storage", onStorage);
      }
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
        if (listeners.size === 0) {
          window.removeEventListener("storage", onStorage);
        }
      };
    },
  };
}

let defaultStore: ThemeStore | undefined;

function themeStore(): ThemeStore {
  defaultStore ??= createThemeStore(() => window.localStorage, document.documentElement);
  return defaultStore;
}

function systemQuery(): MediaQueryList | undefined {
  return typeof window.matchMedia === "function" ? window.matchMedia(DARK_QUERY) : undefined;
}

function subscribeSystem(listener: () => void): () => void {
  const query = systemQuery();
  query?.addEventListener("change", listener);
  return () => query?.removeEventListener("change", listener);
}

function systemPrefersDark(): boolean {
  return systemQuery()?.matches ?? false;
}

/**
 * The theme preference, the theme it shows right now, and a setter. Every
 * caller and every open tab stays in sync; `theme` follows the OS live while
 * the preference is `system`.
 */
export function useTheme(): {
  preference: ThemePreference;
  theme: Theme;
  setPreference: (preference: ThemePreference) => void;
} {
  const store = themeStore();
  const preference = useSyncExternalStore(store.subscribe, store.get);
  const systemDark = useSyncExternalStore(subscribeSystem, systemPrefersDark);
  return {
    preference,
    theme: resolveTheme(preference, systemDark),
    setPreference: store.set,
  };
}
