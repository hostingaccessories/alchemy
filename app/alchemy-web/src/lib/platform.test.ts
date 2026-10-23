import { afterEach, describe, expect, it } from "vite-plus/test";

import { apiBase, isTauri } from "./platform.ts";

afterEach(() => {
  delete (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__;
});

describe("platform", () => {
  it("is same-origin in a browser", () => {
    expect(isTauri()).toBe(false);
    expect(apiBase()).toBe("");
  });

  it("detects the Tauri shell", () => {
    (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
    expect(isTauri()).toBe(true);
  });
});
