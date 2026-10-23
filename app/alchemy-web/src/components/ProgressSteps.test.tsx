import { cleanup, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it } from "vite-plus/test";

import { ProgressSteps, type Step, type StepState } from "./ProgressSteps.tsx";

afterEach(cleanup);

function items() {
  return within(screen.getByRole("list", { name: "Installing Nextcloud" })).getAllByRole(
    "listitem",
  );
}

describe("ProgressSteps", () => {
  const states: [StepState, string][] = [
    ["pending", "Not started"],
    ["active", "In progress"],
    ["done", "Done"],
    ["failed", "Failed"],
    ["skipped", "Skipped"],
  ];

  it.each(states)("names the %s state for screen readers", (state, text) => {
    render(
      <ProgressSteps
        label="Installing Nextcloud"
        steps={[{ id: "a", label: "Start Nextcloud", state }]}
      />,
    );
    expect(items()[0]?.textContent).toBe(`Start Nextcloud: ${text}`);
  });

  it("lists steps in order and marks only the active one current", () => {
    const steps: Step[] = [
      { id: "check", label: "Check Docker", state: "done" },
      { id: "pull", label: "Download Nextcloud", state: "active", message: "212 MB of 540 MB" },
      { id: "start", label: "Start Nextcloud", state: "pending" },
    ];
    render(<ProgressSteps label="Installing Nextcloud" steps={steps} />);
    const [check, pull, start] = items();
    expect(check?.textContent).toContain("Check Docker");
    expect(pull?.getAttribute("aria-current")).toBe("step");
    expect(pull?.textContent).toContain("212 MB of 540 MB");
    expect(check?.hasAttribute("aria-current")).toBe(false);
    expect(start?.hasAttribute("aria-current")).toBe(false);
  });

  it("shows a failed step's problem and folds its raw output", async () => {
    const user = userEvent.setup();
    render(
      <ProgressSteps
        label="Installing Nextcloud"
        steps={[
          {
            id: "start",
            label: "Start Nextcloud",
            state: "failed",
            message: "Port 8080 is already in use.",
            output: "driver failed programming external connectivity",
          },
        ]}
      />,
    );
    expect(screen.getByText("Port 8080 is already in use.").className).toContain("text-error");
    const toggle = screen.getByRole("button", { name: "Show output" });
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    expect(screen.queryByText("driver failed programming external connectivity")).toBeNull();

    await user.click(toggle);
    expect(screen.getByText("driver failed programming external connectivity")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Copy output" })).toBeTruthy();
  });

  it("has no output toggle for a step without output", () => {
    render(
      <ProgressSteps
        label="Installing Nextcloud"
        steps={[{ id: "a", label: "Check Docker", state: "done" }]}
      />,
    );
    expect(screen.queryByRole("button")).toBeNull();
  });
});
