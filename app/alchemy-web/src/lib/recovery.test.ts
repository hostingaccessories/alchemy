import { describe, expect, it } from "vite-plus/test";

import type { Operation, Reconcile, ReconcileApp, Wake } from "./api.ts";
import { reconcileKey, recoveryReport, sleptFor } from "./recovery.ts";

const AT = 1_790_000_000;

function app(name: string, extra: Partial<ReconcileApp>): ReconcileApp {
  return { app: name, outcome: "skipped", operation: null, reason: null, message: null, ...extra };
}

function reconcile(apps: ReconcileApp[], extra: Partial<Reconcile> = {}): Reconcile {
  return {
    trigger: "wake",
    at: AT,
    reason: "was running before the computer slept",
    apps,
    ...extra,
  };
}

const failedOp: Operation = {
  id: "op-1",
  app: "immich",
  action: "up",
  state: "failed",
  steps: [],
  started_at: AT,
  finished_at: AT + 5,
  error: "port 2283 is already in use",
  reason: "was running before the computer slept",
};

describe("sleptFor", () => {
  const cases: [number, string][] = [
    [1, "1 second"],
    [45, "45 seconds"],
    [120, "2 minutes"],
    [60 * 60, "60 minutes"],
    [2 * 3600, "2 hours"],
    [3 * 86400, "3 days"],
  ];
  it.each(cases)("reads %s seconds as %s", (seconds, want) => {
    expect(sleptFor(seconds)).toBe(want);
  });
});

describe("recoveryReport", () => {
  it("says nothing when no app was started and none needs a look", () => {
    const quiet = reconcile([
      app("blog", { reason: "already_running" }),
      app("wiki", { reason: "not_running_before" }),
      app("notes", { reason: "stop_intent" }),
    ]);
    expect(recoveryReport(quiet, [], null)).toBeNull();
  });

  const titles: [string, Reconcile, Wake | null, string][] = [
    [
      "apps that came back after a wake",
      reconcile([app("blog", { outcome: "started" }), app("wiki", { outcome: "started" })]),
      null,
      "2 apps came back after this computer slept",
    ],
    [
      "how long the computer slept, from its wake",
      reconcile([app("blog", { outcome: "started" })]),
      { slept_for: 7200, at: AT - 10 },
      "1 app came back after this computer slept for 2 hours",
    ],
    [
      "no length from a wake long before",
      reconcile([app("blog", { outcome: "started" })]),
      { slept_for: 7200, at: AT - 86400 },
      "1 app came back after this computer slept",
    ],
    [
      "apps still starting after a restart",
      reconcile([app("blog", { outcome: "starting" }), app("wiki", { outcome: "started" })], {
        trigger: "daemon_start",
      }),
      null,
      "Bringing 1 app back after Alchemy restarted…",
    ],
    [
      "an app that didn't come back after Docker returned",
      reconcile(
        [app("blog", { outcome: "started" }), app("immich", { outcome: "failed", message: "x" })],
        { trigger: "engine_returned" },
      ),
      null,
      "1 app didn't come back after Docker restarted",
    ],
  ];

  it.each(titles)("titles %s", (_name, value, wake, want) => {
    expect(recoveryReport(value, [], wake)?.title).toBe(want);
  });

  it("says why each app didn't come back and what to do", () => {
    const report = recoveryReport(
      reconcile([
        app("blog", { outcome: "started" }),
        app("immich", { outcome: "failed", operation: "op-1" }),
        app("wiki", { outcome: "failed", message: "Docker went away" }),
        app("photos", { reason: "partly_running" }),
        app("mail", { reason: "status_unknown" }),
        app("notes", { reason: "stop_intent" }),
      ]),
      [failedOp],
      null,
    );
    expect(report?.problem).toBe(true);
    expect(report?.cameBack).toEqual(["blog"]);
    expect(report?.keptStopped).toEqual(["notes"]);
    expect(report?.problems).toEqual([
      {
        app: "immich",
        text: "didn't come back: port 2283 is already in use. Open it to see what went wrong, then start it again.",
      },
      {
        app: "wiki",
        text: "didn't come back: Docker went away. Open it to see what went wrong, then start it again.",
      },
      {
        app: "photos",
        text: "is only partly running, so Alchemy left it alone. Open it and select Start to bring the rest back.",
      },
      {
        app: "mail",
        text: "wasn't brought back, because Alchemy couldn't tell whether it was running. Open it to check.",
      },
    ]);
    expect(report?.text.split("\n")).toEqual([
      `Alchemy, after this computer slept (${new Date(AT * 1000).toISOString()}):`,
      "- blog came back.",
      "- immich didn't come back: port 2283 is already in use. Open it to see what went wrong, then start it again.",
      "- wiki didn't come back: Docker went away. Open it to see what went wrong, then start it again.",
      "- photos is only partly running, so Alchemy left it alone. Open it and select Start to bring the rest back.",
      "- mail wasn't brought back, because Alchemy couldn't tell whether it was running. Open it to check.",
      "- notes stayed stopped, because it was stopped in Alchemy.",
    ]);
  });

  it("says a start failed without a reason when there is none", () => {
    const report = recoveryReport(reconcile([app("blog", { outcome: "failed" })]), [], null);
    expect(report?.problems[0]?.text).toBe(
      "didn't come back. Open it to see what went wrong, then start it again.",
    );
  });
});

describe("reconcileKey", () => {
  it("tells reconciles apart by trigger and time", () => {
    expect(reconcileKey(reconcile([]))).toBe(`wake:${AT}`);
    expect(reconcileKey(reconcile([], { trigger: "daemon_start", at: AT + 1 }))).toBe(
      `daemon_start:${AT + 1}`,
    );
  });
});
