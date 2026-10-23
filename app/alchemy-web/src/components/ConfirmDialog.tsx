import * as AlertDialog from "@radix-ui/react-alert-dialog";
import { type ReactNode, useId, useRef, useState } from "react";

import { ApiError } from "../lib/api.ts";
import { Button } from "./Button.tsx";
import { Notice } from "./Notice.tsx";

/** Props for {@link ConfirmDialog}. */
export interface ConfirmDialogProps {
  /** The button that opens the dialog. Rendered as-is, so pass a real button. */
  trigger: ReactNode;
  /** The question, naming the thing affected: "Remove Nextcloud?" */
  title: string;
  /** What happens, and what can't be undone. */
  description: ReactNode;
  /** The destructive button's text, a verb: "Remove app". */
  confirmLabel: string;
  /**
   * Runs the action. The dialog stays open, with both buttons disabled, until
   * a returned promise settles; it closes on success and shows the error
   * otherwise.
   */
  onConfirm: () => void | Promise<void>;
  /**
   * For actions that delete data: the name to type before the confirm button
   * enables, usually the app's name.
   */
  typeToConfirm?: string;
}

/**
 * Ask before a destructive action. Focus is trapped inside while open and
 * starts on Cancel, or on the name field when {@link ConfirmDialogProps.typeToConfirm}
 * is set; Escape cancels and focus returns to the trigger.
 */
export function ConfirmDialog({
  trigger,
  title,
  description,
  confirmLabel,
  onConfirm,
  typeToConfirm,
}: ConfirmDialogProps) {
  const [open, setOpen] = useState(false);
  const [typed, setTyped] = useState("");
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const input = useRef<HTMLInputElement>(null);
  const inputId = useId();

  const matches = typeToConfirm === undefined || typed.trim() === typeToConfirm;

  const onOpenChange = (next: boolean) => {
    if (pending) {
      return;
    }
    setOpen(next);
    setTyped("");
    setError(null);
  };

  const confirm = async () => {
    if (!matches) {
      return;
    }
    setPending(true);
    setError(null);
    try {
      await onConfirm();
      setPending(false);
      setOpen(false);
      setTyped("");
    } catch (err) {
      setPending(false);
      setError(err instanceof ApiError ? err.message : "Something went wrong. Try again.");
    }
  };

  return (
    <AlertDialog.Root open={open} onOpenChange={onOpenChange}>
      <AlertDialog.Trigger asChild>{trigger}</AlertDialog.Trigger>
      <AlertDialog.Portal>
        <AlertDialog.Overlay className="fixed inset-0 bg-surface/70 backdrop-blur-sm" />
        <AlertDialog.Content
          className="fixed top-1/2 left-1/2 flex w-[calc(100%-2rem)] max-w-md -translate-x-1/2 -translate-y-1/2 flex-col gap-4 rounded-lg border border-border bg-surface-raised p-6 text-text shadow-xl"
          onOpenAutoFocus={(event) => {
            if (input.current) {
              event.preventDefault();
              input.current.focus();
            }
          }}
        >
          <AlertDialog.Title className="text-lg font-semibold">{title}</AlertDialog.Title>
          <AlertDialog.Description asChild>
            <div className="flex flex-col gap-2 text-sm text-muted">{description}</div>
          </AlertDialog.Description>
          {typeToConfirm !== undefined && (
            <form
              className="flex flex-col gap-1"
              onSubmit={(event) => {
                event.preventDefault();
                void confirm();
              }}
            >
              <label htmlFor={inputId} className="text-sm">
                Type <strong className="font-semibold">{typeToConfirm}</strong> to confirm
              </label>
              <input
                ref={input}
                id={inputId}
                value={typed}
                onChange={(event) => setTyped(event.target.value)}
                disabled={pending}
                autoComplete="off"
                spellCheck={false}
                className="rounded-md border border-border bg-surface px-2 py-1.5 text-text focus-visible:ring-2 focus-visible:ring-accent focus-visible:outline-none"
              />
            </form>
          )}
          {error && (
            <div role="alert">
              <Notice tone="error" title={error} />
            </div>
          )}
          <div className="flex justify-end gap-2">
            <AlertDialog.Cancel asChild>
              <Button disabled={pending}>Cancel</Button>
            </AlertDialog.Cancel>
            <Button variant="danger" disabled={!matches || pending} onClick={() => void confirm()}>
              {confirmLabel}
            </Button>
          </div>
        </AlertDialog.Content>
      </AlertDialog.Portal>
    </AlertDialog.Root>
  );
}
