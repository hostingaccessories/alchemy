import { describe, expect, it } from "vite-plus/test";

import type { StopReason } from "./api.ts";
import { describeStopReason, isStopProblem } from "./stopReason.ts";

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
});
