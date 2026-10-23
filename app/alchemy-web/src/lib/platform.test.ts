import { afterEach, beforeEach, describe, expect, it, vi } from "vite-plus/test";

import { apiBase, externalUrl, isTauri, openExternal } from "./platform.ts";

const tauri = vi.hoisted(() => ({
  openUrl: vi.fn(async (_url: string) => {}),
}));

vi.mock("@tauri-apps/plugin-opener", () => ({ openUrl: tauri.openUrl }));

function enterShell() {
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
}

let open: ReturnType<typeof vi.fn>;

beforeEach(() => {
  open = vi.fn();
  vi.stubGlobal("open", open);
});

afterEach(() => {
  delete (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__;
  vi.unstubAllGlobals();
  vi.clearAllMocks();
});

describe("platform", () => {
  it("is same-origin in a browser", () => {
    expect(isTauri()).toBe(false);
    expect(apiBase()).toBe("");
  });

  it("detects the Tauri shell", () => {
    enterShell();
    expect(isTauri()).toBe(true);
  });
});

describe("externalUrl", () => {
  it.each([
    ["https://docs.docker.com/get-started/", "https://docs.docker.com/get-started/"],
    ["HTTPS://Example.com", "https://example.com/"],
    ["http://example.com/", null],
    ["javascript:alert(1)", null],
    ["mailto:me@example.com", null],
    ["file:///etc/passwd", null],
    ["tauri://localhost/", null],
    ["/apps", null],
    ["", null],
    ["not a url", null],
  ])("%j -> %j", (url, expected) => {
    expect(externalUrl(url)?.href ?? null).toBe(expected);
  });
});

describe("openExternal", () => {
  it("opens a new tab with no opener or referrer in a browser", async () => {
    await openExternal("https://example.com/docs");
    expect(open).toHaveBeenCalledWith("https://example.com/docs", "_blank", "noopener,noreferrer");
    expect(tauri.openUrl).not.toHaveBeenCalled();
  });

  it("hands the URL to the opener plugin in the shell", async () => {
    enterShell();
    await openExternal("https://example.com/docs");
    expect(tauri.openUrl).toHaveBeenCalledWith("https://example.com/docs");
    expect(open).not.toHaveBeenCalled();
  });

  it.each(["http://example.com/", "javascript:alert(1)", "file:///etc/passwd", "/apps"])(
    "rejects %j and opens nothing",
    async (url) => {
      await expect(openExternal(url)).rejects.toThrow("Only https: links open outside Alchemy");
      enterShell();
      await expect(openExternal(url)).rejects.toThrow("Only https: links open outside Alchemy");
      expect(open).not.toHaveBeenCalled();
      expect(tauri.openUrl).not.toHaveBeenCalled();
    },
  );
});
