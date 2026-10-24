import { cleanup, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vite-plus/test";

import { ApiError, NO_RESPONSE } from "../lib/api.ts";
import { ErrorState, type ErrorSummary, summarizeError } from "./ErrorState.tsx";

afterEach(cleanup);

describe("summarizeError", () => {
  const cases: [string, unknown, string | undefined, ErrorSummary][] = [
    [
      "an unreachable daemon",
      new ApiError(NO_RESPONSE, "alchemyd is not reachable"),
      "Couldn't load your apps",
      {
        title: "Alchemy can't reach alchemyd",
        message: "Check that alchemyd is running on this machine, then try again.",
        detail: "alchemyd did not respond: alchemyd is not reachable",
      },
    ],
    [
      "a daemon error",
      new ApiError(500, "no space left on device"),
      "Couldn't load your apps",
      {
        title: "Couldn't load your apps",
        message: "no space left on device",
        detail: "HTTP 500: no space left on device",
      },
    ],
    [
      "a browser that isn't signed in",
      new ApiError(401, "not signed in", "unauthorized"),
      "Couldn't load your apps",
      {
        title: "You're signed out of alchemyd",
        message: "Run `alchemy open` in a terminal on this machine to sign in, then try again.",
        detail: "HTTP 401: not signed in",
      },
    ],
    [
      "a 401 without the unauthorized code",
      new ApiError(401, "who are you"),
      "Couldn't load your apps",
      {
        title: "Couldn't load your apps",
        message: "who are you",
        detail: "HTTP 401: who are you",
      },
    ],
    [
      "a daemon error without a title",
      new ApiError(404, "no app named nextcloud"),
      undefined,
      {
        title: "Something went wrong",
        message: "no app named nextcloud",
        detail: "HTTP 404: no app named nextcloud",
      },
    ],
    [
      "a bug",
      new TypeError("x is undefined"),
      undefined,
      {
        title: "Something went wrong",
        message: "This is a bug in Alchemy. Try again, or copy the details.",
        detail: "TypeError: x is undefined",
      },
    ],
    [
      "a thrown string",
      "boom",
      undefined,
      {
        title: "Something went wrong",
        message: "This is a bug in Alchemy. Try again, or copy the details.",
        detail: "boom",
      },
    ],
  ];

  it.each(cases)("summarizes %s", (_name, error, title, expected) => {
    expect(summarizeError(error, title)).toEqual(expected);
  });

  it("says the desktop shell couldn't sign in, since it has nothing to run", () => {
    (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
    try {
      expect(summarizeError(new ApiError(401, "not signed in", "unauthorized"))).toEqual({
        title: "You're signed out of alchemyd",
        message:
          "Alchemy couldn't sign in to alchemyd. Check that alchemyd is running as you, then try again.",
        detail: "HTTP 401: not signed in",
      });
    } finally {
      delete (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__;
    }
  });
});

describe("ErrorState", () => {
  it("announces the problem and the daemon's message", () => {
    render(<ErrorState error={new ApiError(500, "disk full")} title="Couldn't load your apps" />);
    const alert = screen.getByRole("alert");
    expect(within(alert).getByText("Couldn't load your apps")).toBeTruthy();
    expect(within(alert).getByText("disk full")).toBeTruthy();
  });

  it("does not show a bug's internal message until asked", async () => {
    const user = userEvent.setup();
    render(<ErrorState error={new Error("secret internals")} />);
    expect(screen.queryByText("Error: secret internals")).toBeNull();

    await user.click(screen.getByRole("button", { name: "Details" }));
    expect(screen.getByText("Error: secret internals")).toBeTruthy();
  });

  it("retries", async () => {
    const user = userEvent.setup();
    const onRetry = vi.fn();
    render(<ErrorState error={new ApiError(NO_RESPONSE, "unreachable")} onRetry={onRetry} />);
    await user.click(screen.getByRole("button", { name: "Try again" }));
    expect(onRetry).toHaveBeenCalledOnce();
  });

  it("has no retry without a handler", () => {
    render(<ErrorState error={new ApiError(500, "disk full")} />);
    expect(screen.queryByRole("button", { name: "Try again" })).toBeNull();
  });

  it("copies the detail", async () => {
    const user = userEvent.setup();
    const writeText = vi.spyOn(navigator.clipboard, "writeText");
    render(<ErrorState error={new ApiError(500, "disk full")} />);
    await user.click(screen.getByRole("button", { name: "Details" }));
    await user.click(screen.getByRole("button", { name: "Copy details" }));
    expect(writeText).toHaveBeenCalledWith("HTTP 500: disk full");
  });
});
