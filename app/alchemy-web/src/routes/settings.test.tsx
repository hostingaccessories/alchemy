import { act, cleanup, fireEvent, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vite-plus/test";

import { THEME_STORAGE_KEY } from "../lib/theme.ts";
import { stubSystemTheme } from "../test/media.ts";
import { FakeEventSource, renderAt } from "../test/render.tsx";

const root = document.documentElement;

const setTheme = vi.hoisted(() => vi.fn(async (_theme: "light" | "dark" | null) => {}));

vi.mock("@tauri-apps/api/window", () => ({ getCurrentWindow: () => ({ setTheme }) }));

beforeEach(() => {
  vi.stubGlobal("EventSource", FakeEventSource);
  vi.stubGlobal(
    "fetch",
    vi.fn(async () => new Response(JSON.stringify({ version: "9.9.9", api: 1 }), { status: 200 })),
  );
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  localStorage.clear();
  delete root.dataset.theme;
  delete (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__;
  setTheme.mockClear();
});

function checked() {
  return screen
    .getAllByRole<HTMLInputElement>("radio")
    .filter((radio) => radio.checked)
    .map((radio) => radio.value);
}

describe("theme setting", () => {
  it("defaults to System and follows the OS", async () => {
    const setSystemDark = stubSystemTheme(true);
    await renderAt("/settings");

    expect(checked()).toEqual(["system"]);
    expect(screen.getByText(/which is dark right now/)).toBeTruthy();
    expect(root.dataset.theme).toBeUndefined();

    act(() => setSystemDark(false));
    expect(screen.getByText(/which is light right now/)).toBeTruthy();
    expect(root.dataset.theme).toBeUndefined();
  });

  it("overrides the OS and remembers the choice", async () => {
    stubSystemTheme(true);
    await renderAt("/settings");

    fireEvent.click(screen.getByRole("radio", { name: "Light" }));
    expect(checked()).toEqual(["light"]);
    expect(root.dataset.theme).toBe("light");
    expect(localStorage.getItem(THEME_STORAGE_KEY)).toBe("light");
    expect(screen.queryByText(/right now/)).toBeNull();

    fireEvent.click(screen.getByRole("radio", { name: "System" }));
    expect(checked()).toEqual(["system"]);
    expect(root.dataset.theme).toBeUndefined();
    expect(localStorage.getItem(THEME_STORAGE_KEY)).toBeNull();
  });

  it("shows a stored override", async () => {
    stubSystemTheme(false);
    localStorage.setItem(THEME_STORAGE_KEY, "dark");
    await renderAt("/settings");
    expect(checked()).toEqual(["dark"]);
  });

  it("treats a missing matchMedia as light", async () => {
    await renderAt("/settings");
    expect(screen.getByText(/which is light right now/)).toBeTruthy();
  });

  it("leaves the window alone in a browser", async () => {
    await renderAt("/settings");
    fireEvent.click(screen.getByRole("radio", { name: "Dark" }));
    expect(setTheme).not.toHaveBeenCalled();
  });

  it("matches the desktop window's title bar to the choice", async () => {
    (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
    localStorage.setItem(THEME_STORAGE_KEY, "light");
    await renderAt("/settings");
    await vi.waitFor(() => expect(setTheme).toHaveBeenLastCalledWith("light"));

    fireEvent.click(screen.getByRole("radio", { name: "Dark" }));
    await vi.waitFor(() => expect(setTheme).toHaveBeenLastCalledWith("dark"));

    fireEvent.click(screen.getByRole("radio", { name: "System" }));
    await vi.waitFor(() => expect(setTheme).toHaveBeenLastCalledWith(null));
  });
});
