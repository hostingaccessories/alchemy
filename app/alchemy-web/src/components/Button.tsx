import type { ComponentPropsWithRef } from "react";

/** How a {@link Button} looks: the main action, a secondary one, or a destructive one. */
export type ButtonVariant = "primary" | "secondary" | "danger";

const VARIANTS: Record<ButtonVariant, string> = {
  primary: "bg-accent text-accent-contrast hover:bg-accent/90",
  secondary: "border border-border bg-surface-raised text-text hover:bg-surface-muted",
  danger: "bg-error text-surface hover:bg-error/90",
};

/**
 * A plain button in one of the app's styles. Defaults to `type="button"` so
 * it never submits a form by accident.
 */
export function Button({
  variant = "secondary",
  className = "",
  type = "button",
  ...props
}: ComponentPropsWithRef<"button"> & { variant?: ButtonVariant }) {
  return (
    <button
      type={type}
      className={`inline-flex items-center justify-center gap-2 rounded-md px-3 py-1.5 text-sm font-medium focus-visible:ring-2 focus-visible:ring-accent focus-visible:outline-none disabled:cursor-not-allowed disabled:opacity-50 ${VARIANTS[variant]} ${className}`}
      {...props}
    />
  );
}
