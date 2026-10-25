import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, renderHook } from "@testing-library/react";
import { createElement, type ReactNode } from "react";
import { afterEach, describe, expect, it, vi } from "vite-plus/test";

import type { Privilege, PrivilegeList } from "./api.ts";
import {
  RESTART_GRACE_MS,
  expectRestart,
  restartDone,
  restartsSoon,
  useRestart,
} from "./restart.ts";

function privilege(extra: Partial<Privilege>): Privilege {
  return {
    action: "low_ports",
    state: "pending",
    reason: null,
    message: null,
    command: null,
    restarts: true,
    last: null,
    ...extra,
  };
}

const done = { grant: true, result: "done" as const, message: null, at: 1 };

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

describe("restartsSoon", () => {
  const cases: [string, Partial<Privilege>, boolean][] = [
    ["a pending change that succeeded", { last: done }, true],
    ["a prompt still up", {}, false],
    ["a granted action", { state: "granted", last: done }, false],
    ["an action that doesn't restart", { restarts: false, last: done }, false],
    ["a cancelled prompt", { last: { ...done, result: "cancelled" } }, false],
  ];

  it.each(cases)("is %s: %s", (_name, extra, want) => {
    const list: PrivilegeList = { privileges: [privilege(extra)] };
    expect(restartsSoon(list)).toBe(want);
  });
});

describe("useRestart", () => {
  function setup() {
    const client = new QueryClient();
    const wrapper = ({ children }: { children: ReactNode }) =>
      createElement(QueryClientProvider, { client }, children);
    const hook = renderHook(() => useRestart(), { wrapper });
    return { client, hook };
  }

  it("expects nothing at first", () => {
    const { hook } = setup();
    expect(hook.result.current).toEqual({ restarting: false, restarted: false });
  });

  /** Query notifies on a zero-delay timer, so each step advances the fake clock. */
  it("is restarting until the grace period ends, and restarted until done", () => {
    vi.useFakeTimers();
    const { client, hook } = setup();
    act(() => {
      expectRestart(client);
      vi.advanceTimersByTime(0);
    });
    expect(hook.result.current).toEqual({ restarting: true, restarted: true });

    act(() => {
      vi.advanceTimersByTime(RESTART_GRACE_MS + 1);
    });
    expect(hook.result.current).toEqual({ restarting: false, restarted: true });

    act(() => {
      restartDone(client);
      vi.advanceTimersByTime(0);
    });
    expect(hook.result.current).toEqual({ restarting: false, restarted: false });
  });
});
