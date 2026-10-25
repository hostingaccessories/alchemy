import { describe, expect, it } from "vite-plus/test";

import type { EngineState, EngineStatus } from "./api.ts";
import { isDockerAway } from "./engineStart.ts";

function status(state: EngineState): EngineStatus {
  return {
    state,
    flavor: null,
    engine_version: null,
    api_version: null,
    compose_version: null,
    desktop_version: null,
    endpoint: null,
  };
}

describe("isDockerAway", () => {
  const cases: [string, EngineStatus | null | undefined, boolean][] = [
    ["no report yet", undefined, false],
    ["a dropped stream", null, false],
    ["ready", status("ready"), false],
    ["not installed", status("not_installed"), true],
    ["not running", status("not_running"), true],
    ["starting", status("starting"), true],
    ["unsupported", status("unsupported"), true],
    ["a state this build doesn't know", status("paused" as EngineState), false],
  ];

  it.each(cases)("is %s: %s", (_name, value, want) => {
    expect(isDockerAway(value)).toBe(want);
  });
});
