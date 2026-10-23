import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it } from "vite-plus/test";

import { Advanced } from "./Advanced.tsx";

afterEach(cleanup);

describe("Advanced", () => {
  it("starts closed, with the trigger saying so", () => {
    render(<Advanced>Compose file path</Advanced>);
    const trigger = screen.getByRole("button", { name: "Advanced" });
    expect(trigger.getAttribute("aria-expanded")).toBe("false");
    expect(screen.queryByText("Compose file path")).toBeNull();
  });

  it("opens and closes from the keyboard", async () => {
    const user = userEvent.setup();
    render(<Advanced label="Show output">raw output</Advanced>);
    const trigger = screen.getByRole("button", { name: "Show output" });

    await user.tab();
    expect(document.activeElement).toBe(trigger);
    await user.keyboard("{Enter}");
    expect(trigger.getAttribute("aria-expanded")).toBe("true");
    const content = screen.getByText("raw output");
    expect(trigger.getAttribute("aria-controls")).toBe(content.id);

    await user.keyboard(" ");
    expect(trigger.getAttribute("aria-expanded")).toBe("false");
    expect(screen.queryByText("raw output")).toBeNull();
  });

  it("can start open", () => {
    render(<Advanced defaultOpen>Ports</Advanced>);
    expect(screen.getByRole("button").getAttribute("aria-expanded")).toBe("true");
    expect(screen.getByText("Ports")).toBeTruthy();
  });
});
