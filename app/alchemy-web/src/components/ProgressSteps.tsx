import {
  Circle,
  CircleCheck,
  CircleMinus,
  CircleX,
  LoaderCircle,
  type LucideIcon,
} from "lucide-react";

import { Advanced } from "./Advanced.tsx";
import { CopyButton } from "./CopyButton.tsx";

/** Where one {@link Step} is. */
export type StepState = "pending" | "active" | "done" | "failed" | "skipped";

/** One plain-language step of a longer operation. */
export interface Step {
  /** Stable key for the step. */
  id: string;
  /** What the step does, for a person: "Download Nextcloud". */
  label: string;
  state: StepState;
  /** One line under the label: the problem when failed, or progress when active. */
  message?: string;
  /** Raw output (logs, command output), folded until asked for. */
  output?: string;
}

const STATES: Record<StepState, { icon: LucideIcon; className: string; text: string }> = {
  pending: { icon: Circle, className: "text-muted", text: "Not started" },
  active: { icon: LoaderCircle, className: "animate-spin text-accent", text: "In progress" },
  done: { icon: CircleCheck, className: "text-ok", text: "Done" },
  failed: { icon: CircleX, className: "text-error", text: "Failed" },
  skipped: { icon: CircleMinus, className: "text-muted", text: "Skipped" },
};

/**
 * A multi-step operation as a list a newcomer can follow: each step's state,
 * a one-line message, and its raw output folded underneath. The active step
 * carries `aria-current="step"`. Props only for now; the daemon's progress
 * events land with #7.
 */
export function ProgressSteps({ label, steps }: { label: string; steps: Step[] }) {
  return (
    <ol aria-label={label} className="flex flex-col gap-3">
      {steps.map((step) => {
        const { icon: Icon, className, text } = STATES[step.state];
        return (
          <li
            key={step.id}
            aria-current={step.state === "active" ? "step" : undefined}
            className="flex gap-3"
          >
            <Icon className={`mt-0.5 size-4 shrink-0 ${className}`} aria-hidden />
            <div className="flex min-w-0 flex-1 flex-col gap-1">
              <p className={step.state === "pending" ? "text-muted" : "text-text"}>
                {step.label}
                <span className="sr-only">: {text}</span>
              </p>
              {step.message && (
                <p className={`text-sm ${step.state === "failed" ? "text-error" : "text-muted"}`}>
                  {step.message}
                </p>
              )}
              {step.output && (
                <Advanced label="Show output">
                  <pre className="max-h-64 overflow-auto rounded-md bg-surface-muted p-3 text-xs whitespace-pre-wrap text-text">
                    {step.output}
                  </pre>
                  <div>
                    <CopyButton text={step.output} label="Copy output" />
                  </div>
                </Advanced>
              )}
            </div>
          </li>
        );
      })}
    </ol>
  );
}
