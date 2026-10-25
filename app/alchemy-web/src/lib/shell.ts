import { queryOptions, useQuery } from "@tanstack/react-query";

import { isTauri } from "./platform.ts";

/**
 * How the desktop shell stands with alchemyd, from its `launch_state`
 * command. Mirrors `alchemy_gui::launch::Launch` by hand.
 */
export type ShellLaunch =
  | { kind: "checking" | "starting" | "running" | "lost" | "stopped" }
  | { kind: "failed" | "unavailable"; message: string };

/** How often the page asks the shell while it can't reach alchemyd. */
export const SHELL_POLL_MS = 1_000;

const launchQuery = queryOptions({
  queryKey: ["shell-launch"],
  queryFn: async (): Promise<ShellLaunch> => {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke<ShellLaunch>("launch_state");
  },
  refetchInterval: SHELL_POLL_MS,
  retry: false,
});

/**
 * The shell's launch state, asked every second while `enabled`. Always
 * `undefined` in a browser, where there is no shell.
 */
export function useShellLaunch(enabled: boolean): ShellLaunch | undefined {
  return useQuery({ ...launchQuery, enabled: enabled && isTauri() }).data;
}

/** Ask the shell to start alchemyd again. Does nothing in a browser. */
export async function startDaemon(): Promise<void> {
  if (!isTauri()) {
    return;
  }
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("start_daemon");
}
