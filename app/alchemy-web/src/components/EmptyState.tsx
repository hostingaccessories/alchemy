import type { LucideIcon } from "lucide-react";
import type { ReactNode } from "react";

/** A page or list with nothing in it yet: what would be here, and how to add the first one. */
export function EmptyState({
  icon: Icon,
  title,
  children,
  action,
}: {
  icon?: LucideIcon;
  title: string;
  /** What will show up here, in a sentence. */
  children?: ReactNode;
  /** The button that adds the first item. */
  action?: ReactNode;
}) {
  return (
    <div className="flex flex-col items-center gap-2 py-16 text-center">
      {Icon && <Icon className="size-10 text-muted" aria-hidden />}
      <h2 className="text-lg font-semibold">{title}</h2>
      {children && <div className="max-w-sm text-muted">{children}</div>}
      {action && <div className="mt-2">{action}</div>}
    </div>
  );
}
