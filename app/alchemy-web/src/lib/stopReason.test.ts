import { describe, expect, it } from "vite-plus/test";

import type { AppContainer, StackStatus, StopReason } from "./api.ts";
import {
  appStatusLabel,
  describeStopReason,
  isStopProblem,
  serviceStopReason,
} from "./stopReason.ts";

describe("stop reasons", () => {
  it.each<[StopReason | undefined, string, boolean]>([
    [{ kind: "running" }, "Running", false],
    [{ kind: "paused" }, "Paused", false],
    [{ kind: "not_started" }, "Not running", false],
    [{ kind: "stopped_by_alchemy" }, "Stopped", false],
    [{ kind: "stopped_outside" }, "Stopped outside Alchemy", false],
    [{ kind: "finished" }, "Finished", false],
    [{ kind: "unknown" }, "Stopped", false],
    [undefined, "Stopped", false],
    [{ kind: "crashed", exit_code: 1, oom: false }, "Crashed (exit code 1)", true],
    [{ kind: "crashed", exit_code: 137, oom: true }, "Crashed (out of memory)", true],
    [{ kind: "crashed", exit_code: null, oom: false }, "Crashed", true],
    [{ kind: "crash_looping", exit_code: 2, oom: false }, "Keeps crashing (exit code 2)", true],
    [{ kind: "crash_looping", exit_code: null, oom: true }, "Keeps crashing (out of memory)", true],
  ])("describes %j as %s", (reason, words, problem) => {
    expect(describeStopReason(reason)).toBe(words);
    expect(isStopProblem(reason)).toBe(problem);
  });

  const crashed: StopReason = { kind: "crashed", exit_code: 1, oom: false };

  it.each<[StackStatus, StopReason | undefined, string]>([
    ["running", { kind: "running" }, "Running"],
    ["running", crashed, "Running"],
    ["partial", crashed, "Partly running, crashed (exit code 1)"],
    [
      "stopped",
      { kind: "crash_looping", exit_code: null, oom: true },
      "Stopped, keeps crashing (out of memory)",
    ],
    ["stopped", { kind: "stopped_by_alchemy" }, "Stopped"],
    ["stopped", { kind: "stopped_outside" }, "Stopped outside Alchemy"],
    ["partial", { kind: "stopped_outside" }, "Partly running, stopped outside Alchemy"],
    ["stopped", { kind: "finished" }, "Stopped"],
    ["stopped", undefined, "Stopped"],
    ["unknown", crashed, "Status unknown"],
  ])("labels a %s app with %j as %s", (status, reason, want) => {
    expect(appStatusLabel(status, reason)).toBe(want);
  });

  it("puts a service's crash first", () => {
    const container = (state: AppContainer["state"], stop_reason?: StopReason): AppContainer => ({
      id: "a",
      name: "a",
      image: null,
      state,
      health: null,
      stop_reason,
    });
    const service = (...containers: AppContainer[]) => ({ name: "web", containers });
    expect(serviceStopReason(service())).toEqual({ kind: "not_started" });
    expect(serviceStopReason(service(container("exited")))).toEqual({ kind: "unknown" });
    expect(serviceStopReason(service(container("running", crashed)))).toEqual({ kind: "running" });
    expect(
      serviceStopReason(
        service(container("exited", { kind: "finished" }), container("exited", crashed)),
      ),
    ).toEqual(crashed);
  });
});
