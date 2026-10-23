import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vite-plus/test";

import { App } from "./App.tsx";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("App", () => {
  it("shows the daemon version when alchemyd answers", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response(JSON.stringify({ version: "9.9.9" }), { status: 200 })),
    );
    render(<App />);
    expect(await screen.findByText("alchemyd 9.9.9")).toBeTruthy();
  });

  it("reports the daemon as unreachable when the request fails", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => {
        throw new TypeError("connection refused");
      }),
    );
    render(<App />);
    expect(await screen.findByText("alchemyd is not reachable")).toBeTruthy();
  });
});
