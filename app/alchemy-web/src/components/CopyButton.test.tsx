import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vite-plus/test";

import { COPY_FEEDBACK_MS, CopyButton } from "./CopyButton.tsx";

const writeText = vi.fn<(text: string) => Promise<void>>();

beforeEach(() => {
  writeText.mockReset();
  Object.defineProperty(navigator, "clipboard", { value: { writeText }, configurable: true });
  vi.useFakeTimers();
});

afterEach(() => {
  cleanup();
  vi.useRealTimers();
  Reflect.deleteProperty(navigator, "clipboard");
});

async function click(name: string) {
  await act(async () => {
    fireEvent.click(screen.getByRole("button", { name }));
  });
}

describe("CopyButton", () => {
  it("copies the text and says so, then resets", async () => {
    writeText.mockResolvedValue(undefined);
    render(<CopyButton text="HTTP 500: disk full" label="Copy details" />);
    expect(screen.getByRole("status").textContent).toBe("");

    await click("Copy details");
    expect(writeText).toHaveBeenCalledWith("HTTP 500: disk full");
    expect(screen.getByRole("status").textContent).toBe("Copied");

    act(() => {
      vi.advanceTimersByTime(COPY_FEEDBACK_MS);
    });
    expect(screen.getByRole("status").textContent).toBe("");
  });

  it("explains when the clipboard refuses", async () => {
    writeText.mockRejectedValue(new DOMException("denied", "NotAllowedError"));
    render(<CopyButton text="detail" />);
    await click("Copy");
    expect(screen.getByRole("status").textContent).toBe(
      "Couldn't copy. Select the text and copy it instead.",
    );
  });
});
