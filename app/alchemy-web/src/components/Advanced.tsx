import * as Collapsible from "@radix-ui/react-collapsible";
import { ChevronRight } from "lucide-react";
import type { ReactNode } from "react";

/**
 * Folded detail: settings, raw output, or diagnostics that most people never
 * need but nobody should have to hunt for. Closed by default. The trigger
 * carries `aria-expanded` and `aria-controls`.
 */
export function Advanced({
  label = "Advanced",
  defaultOpen = false,
  children,
}: {
  /** The trigger's text. */
  label?: string;
  defaultOpen?: boolean;
  children: ReactNode;
}) {
  return (
    <Collapsible.Root defaultOpen={defaultOpen} className="flex flex-col gap-2">
      <Collapsible.Trigger className="group flex w-fit items-center gap-1 rounded text-sm text-muted hover:text-text focus-visible:ring-2 focus-visible:ring-accent focus-visible:outline-none">
        <ChevronRight
          className="size-4 transition-transform group-data-[state=open]:rotate-90"
          aria-hidden
        />
        {label}
      </Collapsible.Trigger>
      <Collapsible.Content className="flex flex-col gap-2">{children}</Collapsible.Content>
    </Collapsible.Root>
  );
}
