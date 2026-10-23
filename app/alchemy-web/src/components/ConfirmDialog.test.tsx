import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vite-plus/test";

import { ApiError } from "../lib/api.ts";
import { ConfirmDialog, type ConfirmDialogProps } from "./ConfirmDialog.tsx";

afterEach(cleanup);

function renderDialog(props: Partial<ConfirmDialogProps> = {}) {
  const onConfirm = props.onConfirm ?? vi.fn();
  render(
    <ConfirmDialog
      trigger={<button type="button">Remove</button>}
      title="Remove Nextcloud?"
      description="Its containers stop and are removed."
      confirmLabel="Remove app"
      {...props}
      onConfirm={onConfirm}
    />,
  );
  return { onConfirm, user: userEvent.setup() };
}

async function open(user: ReturnType<typeof userEvent.setup>) {
  await user.click(screen.getByRole("button", { name: "Remove" }));
  return screen.getByRole("alertdialog", { name: "Remove Nextcloud?" });
}

describe("ConfirmDialog", () => {
  it("opens as a described alert dialog with focus on Cancel", async () => {
    const { user } = renderDialog();
    const dialog = await open(user);
    expect(dialog.getAttribute("aria-describedby")).toBeTruthy();
    expect(screen.getByText("Its containers stop and are removed.")).toBeTruthy();
    expect(document.activeElement).toBe(screen.getByRole("button", { name: "Cancel" }));
  });

  it("traps focus inside while open", async () => {
    const { user } = renderDialog();
    const dialog = await open(user);
    for (let i = 0; i < 5; i++) {
      await user.tab();
      expect(dialog.contains(document.activeElement)).toBe(true);
    }
    await user.tab({ shift: true });
    expect(dialog.contains(document.activeElement)).toBe(true);
  });

  it("cancels on Escape and returns focus to the trigger", async () => {
    const { user, onConfirm } = renderDialog();
    await open(user);
    await user.keyboard("{Escape}");
    expect(screen.queryByRole("alertdialog")).toBeNull();
    expect(document.activeElement).toBe(screen.getByRole("button", { name: "Remove" }));
    expect(onConfirm).not.toHaveBeenCalled();
  });

  it("cancels from the Cancel button", async () => {
    const { user, onConfirm } = renderDialog();
    await open(user);
    await user.click(screen.getByRole("button", { name: "Cancel" }));
    expect(screen.queryByRole("alertdialog")).toBeNull();
    expect(onConfirm).not.toHaveBeenCalled();
  });

  it("confirms and closes", async () => {
    const { user, onConfirm } = renderDialog();
    await open(user);
    await user.click(screen.getByRole("button", { name: "Remove app" }));
    expect(onConfirm).toHaveBeenCalledOnce();
    expect(screen.queryByRole("alertdialog")).toBeNull();
  });

  it("stays open and disabled while the action runs", async () => {
    let finish: () => void = () => {};
    const onConfirm = vi.fn(() => new Promise<void>((resolve) => (finish = resolve)));
    const { user } = renderDialog({ onConfirm });
    await open(user);
    await user.click(screen.getByRole("button", { name: "Remove app" }));
    expect(screen.getByRole("button", { name: "Remove app" }).hasAttribute("disabled")).toBe(true);
    expect(screen.getByRole("button", { name: "Cancel" }).hasAttribute("disabled")).toBe(true);
    await user.keyboard("{Escape}");
    expect(screen.getByRole("alertdialog")).toBeTruthy();

    finish();
    await vi.waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
  });

  it("shows the daemon's error and stays open when the action fails", async () => {
    const onConfirm = vi.fn(async () => {
      throw new ApiError(409, "Nextcloud is still starting");
    });
    const { user } = renderDialog({ onConfirm });
    await open(user);
    await user.click(screen.getByRole("button", { name: "Remove app" }));
    expect((await screen.findByRole("alert")).textContent).toContain("Nextcloud is still starting");
    expect(screen.getByRole("alertdialog")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Remove app" }).hasAttribute("disabled")).toBe(false);
  });

  it("shows a generic line for other failures", async () => {
    const onConfirm = vi.fn(async () => {
      throw new Error("internal detail");
    });
    const { user } = renderDialog({ onConfirm });
    await open(user);
    await user.click(screen.getByRole("button", { name: "Remove app" }));
    expect((await screen.findByRole("alert")).textContent).toContain(
      "Something went wrong. Try again.",
    );
  });

  describe("typing the name to delete data", () => {
    it("focuses the name field and keeps confirm disabled until it matches", async () => {
      const { user, onConfirm } = renderDialog({ typeToConfirm: "nextcloud" });
      await open(user);
      const field = screen.getByLabelText("Type nextcloud to confirm");
      const confirm = screen.getByRole("button", { name: "Remove app" });
      expect(document.activeElement).toBe(field);
      expect(confirm.hasAttribute("disabled")).toBe(true);

      await user.type(field, "nextclou{Enter}");
      expect(confirm.hasAttribute("disabled")).toBe(true);
      expect(onConfirm).not.toHaveBeenCalled();

      await user.type(field, "d");
      expect(confirm.hasAttribute("disabled")).toBe(false);
      await user.click(confirm);
      expect(onConfirm).toHaveBeenCalledOnce();
    });

    it("confirms with Enter once the name matches", async () => {
      const { user, onConfirm } = renderDialog({ typeToConfirm: "nextcloud" });
      await open(user);
      await user.type(screen.getByLabelText("Type nextcloud to confirm"), "nextcloud{Enter}");
      expect(onConfirm).toHaveBeenCalledOnce();
    });

    it("clears the typed name when reopened", async () => {
      const { user } = renderDialog({ typeToConfirm: "nextcloud" });
      await open(user);
      await user.type(screen.getByLabelText("Type nextcloud to confirm"), "nextcloud");
      await user.keyboard("{Escape}");
      await open(user);
      expect((screen.getByLabelText("Type nextcloud to confirm") as HTMLInputElement).value).toBe(
        "",
      );
    });
  });
});
