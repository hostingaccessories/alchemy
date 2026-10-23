import { afterEach, describe, expect, it } from "vite-plus/test";

import themeScript from "../../public/theme.js?raw";
import {
  THEME_STORAGE_KEY,
  type Theme,
  type ThemePreference,
  applyThemePreference,
  createThemeStore,
  parseThemePreference,
  resolveTheme,
} from "./theme.ts";

const root = document.documentElement;

afterEach(() => {
  localStorage.clear();
  delete root.dataset.theme;
});

/** A `localStorage` stand-in whose every access throws, as when site data is blocked. */
const blocked = () => {
  throw new DOMException("blocked", "SecurityError");
};

describe("parseThemePreference", () => {
  const cases: [string | null, ThemePreference][] = [
    ["light", "light"],
    ["dark", "dark"],
    ["system", "system"],
    [null, "system"],
    ["", "system"],
    ["Dark", "system"],
    ["sepia", "system"],
  ];
  it.each(cases)("reads %j as %s", (raw, want) => {
    expect(parseThemePreference(raw)).toBe(want);
  });
});

describe("resolveTheme", () => {
  const cases: [ThemePreference, boolean, Theme][] = [
    ["system", false, "light"],
    ["system", true, "dark"],
    ["light", false, "light"],
    ["light", true, "light"],
    ["dark", false, "dark"],
    ["dark", true, "dark"],
  ];
  it.each(cases)("shows %s with system dark=%s as %s", (preference, systemDark, want) => {
    expect(resolveTheme(preference, systemDark)).toBe(want);
  });
});

describe("applyThemePreference", () => {
  const cases: [ThemePreference, string | undefined][] = [
    ["light", "light"],
    ["dark", "dark"],
    ["system", undefined],
  ];
  it.each(cases)("sets data-theme for %s", (preference, want) => {
    root.dataset.theme = "dark";
    applyThemePreference(root, preference);
    expect(root.dataset.theme).toBe(want);
  });
});

describe("createThemeStore", () => {
  it("defaults to system", () => {
    expect(createThemeStore(() => localStorage, root).get()).toBe("system");
  });

  it("persists an override and applies it", () => {
    const store = createThemeStore(() => localStorage, root);
    let calls = 0;
    store.subscribe(() => calls++);

    store.set("dark");
    expect(store.get()).toBe("dark");
    expect(localStorage.getItem(THEME_STORAGE_KEY)).toBe("dark");
    expect(root.dataset.theme).toBe("dark");

    store.set("system");
    expect(store.get()).toBe("system");
    expect(localStorage.getItem(THEME_STORAGE_KEY)).toBeNull();
    expect(root.dataset.theme).toBeUndefined();
    expect(calls).toBe(2);
  });

  it("still applies a choice when storage throws", () => {
    const store = createThemeStore(blocked, root);
    expect(store.get()).toBe("system");
    store.set("light");
    expect(store.get()).toBe("light");
    expect(root.dataset.theme).toBe("light");
  });

  it("follows a change made in another tab", () => {
    const store = createThemeStore(() => localStorage, root);
    let calls = 0;
    const unsubscribe = store.subscribe(() => calls++);

    localStorage.setItem(THEME_STORAGE_KEY, "light");
    window.dispatchEvent(new StorageEvent("storage", { key: THEME_STORAGE_KEY }));
    expect(store.get()).toBe("light");
    expect(root.dataset.theme).toBe("light");
    expect(calls).toBe(1);

    unsubscribe();
    window.dispatchEvent(new StorageEvent("storage", { key: THEME_STORAGE_KEY }));
    expect(calls).toBe(1);
  });
});

describe("public/theme.js", () => {
  const run = () => {
    const script = document.createElement("script");
    script.textContent = themeScript;
    document.head.append(script);
    script.remove();
  };

  const cases: [string | null, string | undefined][] = [
    ["dark", "dark"],
    ["light", "light"],
    [null, undefined],
    ["sepia", undefined],
  ];
  it.each(cases)("applies stored %j before paint", (stored, want) => {
    if (stored !== null) {
      localStorage.setItem(THEME_STORAGE_KEY, stored);
    }
    run();
    expect(root.dataset.theme).toBe(want);
  });

  it("agrees with the store on the key", () => {
    createThemeStore(() => localStorage, root).set("dark");
    delete root.dataset.theme;
    run();
    expect(root.dataset.theme).toBe("dark");
  });
});
