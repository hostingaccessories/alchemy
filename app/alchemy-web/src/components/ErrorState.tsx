import { RotateCw } from "lucide-react";

import { ApiError, NO_RESPONSE, isSignedOut } from "../lib/api.ts";
import { isTauri } from "../lib/platform.ts";
import { Button } from "./Button.tsx";
import { CopyButton } from "./CopyButton.tsx";
import { Notice } from "./Notice.tsx";

/** What {@link ErrorState} shows for one error. */
export interface ErrorSummary {
  /** The problem, in one line. */
  title: string;
  /** The next step, or the daemon's own message. */
  message: string;
  /** A line to paste into a chat or an issue. */
  detail: string;
}

/**
 * Turn any thrown value into something safe to show. An {@link ApiError}'s
 * message is the daemon's own text and always shown; another error's message
 * may be internal, so it only goes into the copyable detail.
 */
export function summarizeError(error: unknown, title = "Something went wrong"): ErrorSummary {
  if (error instanceof ApiError) {
    if (error.status === NO_RESPONSE) {
      return {
        title: "Alchemy can't reach alchemyd",
        message: "Check that alchemyd is running on this machine, then try again.",
        detail: `alchemyd did not respond: ${error.message}`,
      };
    }
    if (isSignedOut(error)) {
      return {
        title: "You're signed out of alchemyd",
        message: isTauri()
          ? "Alchemy couldn't sign in to alchemyd. Check that alchemyd is running as you, then try again."
          : "Run `alchemy open` in a terminal on this machine to sign in, then try again.",
        detail: `HTTP ${error.status}: ${error.message}`,
      };
    }
    return { title, message: error.message, detail: `HTTP ${error.status}: ${error.message}` };
  }
  let detail = "Unknown error";
  if (error instanceof Error) {
    detail = `${error.name}: ${error.message}`;
  } else if (typeof error === "string") {
    detail = error;
  }
  return { title, message: "This is a bug in Alchemy. Try again, or copy the details.", detail };
}

/**
 * A page or panel that failed to load: the problem, a retry, and the detail
 * folded with a copy button. Pass a query's `error` and `refetch` straight
 * through. Announced as an alert.
 */
export function ErrorState({
  error,
  title,
  onRetry,
}: {
  error: unknown;
  /** What failed, from the page's point of view: "Couldn't load your apps". */
  title?: string;
  onRetry?: () => void;
}) {
  const summary = summarizeError(error, title);
  return (
    <div role="alert" className="py-4">
      <Notice
        tone="error"
        title={summary.title}
        action={
          onRetry && (
            <Button onClick={onRetry}>
              <RotateCw className="size-4" aria-hidden />
              Try again
            </Button>
          )
        }
        detail={
          <>
            <pre className="overflow-auto rounded-md bg-surface-muted p-3 text-xs whitespace-pre-wrap text-text">
              {summary.detail}
            </pre>
            <div>
              <CopyButton text={summary.detail} label="Copy details" />
            </div>
          </>
        }
      >
        <p>{summary.message}</p>
      </Notice>
    </div>
  );
}
