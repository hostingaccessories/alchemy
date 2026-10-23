import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Boxes } from "lucide-react";
import { afterEach, describe, expect, it } from "vite-plus/test";

import { EmptyState } from "./EmptyState.tsx";
import { Loading } from "./Loading.tsx";
import { Notice, type Tone } from "./Notice.tsx";

afterEach(cleanup);

describe("Notice", () => {
  const tones: [Tone, string][] = [
    ["ok", "text-ok"],
    ["warn", "text-warn"],
    ["error", "text-error"],
    ["busy", "text-muted"],
  ];

  it.each(tones)("shows a %s title in its colour", (tone, className) => {
    render(<Notice tone={tone} title="Docker is ready" />);
    expect(screen.getByText("Docker is ready").className).toContain(className);
  });

  it("labels the busy spinner and hides the other icons", () => {
    const { container, rerender } = render(<Notice tone="busy" title="Starting…" />);
    expect(screen.getByRole("img", { name: "In progress" })).toBeTruthy();
    rerender(<Notice tone="error" title="Stopped" />);
    expect(screen.queryByRole("img")).toBeNull();
    expect(container.querySelector("svg")?.getAttribute("aria-hidden")).toBe("true");
  });

  it("shows the next step and action, and folds the detail", async () => {
    const user = userEvent.setup();
    render(
      <Notice
        tone="error"
        title="Nextcloud stopped"
        action={<button type="button">Start it</button>}
        detail="exit code 137"
      >
        Start it again to keep using it.
      </Notice>,
    );
    expect(screen.getByText("Start it again to keep using it.")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Start it" })).toBeTruthy();
    expect(screen.queryByText("exit code 137")).toBeNull();

    await user.click(screen.getByRole("button", { name: "Details" }));
    expect(screen.getByText("exit code 137")).toBeTruthy();
  });

  it("has no details toggle without detail", () => {
    render(<Notice tone="ok" title="Docker is ready" />);
    expect(screen.queryByRole("button")).toBeNull();
  });
});

describe("EmptyState", () => {
  it("shows the title, explanation, and first action", () => {
    render(
      <EmptyState icon={Boxes} title="No apps yet" action={<button type="button">Add app</button>}>
        Apps you add will show up here.
      </EmptyState>,
    );
    expect(screen.getByRole("heading", { name: "No apps yet" })).toBeTruthy();
    expect(screen.getByText("Apps you add will show up here.")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Add app" })).toBeTruthy();
  });
});

describe("Loading", () => {
  it("announces what is loading", () => {
    render(<Loading label="Loading apps…" />);
    expect(screen.getByRole("status").textContent).toBe("Loading apps…");
  });

  it("has a default label", () => {
    render(<Loading />);
    expect(screen.getByRole("status").textContent).toBe("Loading…");
  });
});
