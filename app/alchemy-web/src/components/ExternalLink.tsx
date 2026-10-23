import type { ReactNode } from "react";

import { openExternal } from "../lib/platform.ts";

/**
 * A link to a page outside Alchemy, opened in the default browser through
 * {@link openExternal}, in the desktop shell too. Use it for every external
 * link; a plain `<a target="_blank">` does nothing in the shell. Only
 * `https:` URLs are accepted.
 */
export function ExternalLink({
  href,
  children,
}: {
  href: `https://${string}`;
  children: ReactNode;
}) {
  return (
    <a
      className="text-accent underline"
      href={href}
      target="_blank"
      rel="noopener noreferrer"
      onClick={(event) => {
        event.preventDefault();
        openExternal(href).catch((error: unknown) => {
          console.error("Couldn't open link", error);
        });
      }}
    >
      {children}
    </a>
  );
}
