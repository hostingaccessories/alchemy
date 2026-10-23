import { type ReactNode, createContext, useContext } from "react";
import { createPortal } from "react-dom";

/** The layout header's slot element, or `null` before it mounts. */
export const HeaderSlotContext = createContext<HTMLElement | null>(null);

/**
 * Fill the layout header for the current page: a title, and optional
 * actions on the right. Render one per page; it portals into the header
 * the layout provides.
 */
export function PageHeader({ title, children }: { title: string; children?: ReactNode }) {
  const slot = useContext(HeaderSlotContext);
  if (!slot) {
    return null;
  }
  return createPortal(
    <>
      <h1 className="min-w-0 flex-1 truncate text-lg font-semibold">{title}</h1>
      {children && <div className="flex shrink-0 items-center gap-2">{children}</div>}
    </>,
    slot,
  );
}
