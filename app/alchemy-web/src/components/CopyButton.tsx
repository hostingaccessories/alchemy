import { Check, Copy } from "lucide-react";
import { useEffect, useState } from "react";

import { Button } from "./Button.tsx";

/** How long the button shows its result before resetting. */
export const COPY_FEEDBACK_MS = 2000;

type Result = "idle" | "copied" | "failed";

/**
 * Copy `text` to the clipboard, so an error or diagnostic pastes cleanly into
 * a chat or an issue. The result is announced through a status region.
 */
export function CopyButton({ text, label = "Copy" }: { text: string; label?: string }) {
  const [result, setResult] = useState<Result>("idle");

  useEffect(() => {
    if (result === "idle") {
      return;
    }
    const timer = setTimeout(() => setResult("idle"), COPY_FEEDBACK_MS);
    return () => clearTimeout(timer);
  }, [result]);

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(text);
      setResult("copied");
    } catch {
      setResult("failed");
    }
  };

  const Icon = result === "copied" ? Check : Copy;
  return (
    <span className="inline-flex items-center gap-2">
      <Button onClick={copy}>
        <Icon className="size-4" aria-hidden />
        {label}
      </Button>
      <span role="status" className="text-sm text-muted">
        {result === "copied" && "Copied"}
        {result === "failed" && "Couldn't copy. Select the text and copy it instead."}
      </span>
    </span>
  );
}
