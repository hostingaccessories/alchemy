import type { ReactNode } from "react";

import { openAddress, openExternal } from "../lib/platform.ts";

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
    <OpenLink href={href} open={openExternal} className="text-accent underline">
      {children}
    </OpenLink>
  );
}

/**
 * A link to one of an app's web addresses, opened in the default browser
 * through {@link openAddress}. Unlike {@link ExternalLink} it takes `http:`,
 * since apps are served over plain HTTP. `href` must come from alchemyd.
 */
export function AddressLink({
  href,
  className = "text-accent underline",
  children,
}: {
  href: string;
  className?: string;
  children: ReactNode;
}) {
  return (
    <OpenLink href={href} open={openAddress} className={className}>
      {children}
    </OpenLink>
  );
}

function OpenLink({
  href,
  open,
  className,
  children,
}: {
  href: string;
  open: (url: string) => Promise<void>;
  className: string;
  children: ReactNode;
}) {
  return (
    <a
      className={className}
      href={href}
      target="_blank"
      rel="noopener noreferrer"
      onClick={(event) => {
        event.preventDefault();
        open(href).catch((error: unknown) => {
          console.error("Couldn't open link", error);
        });
      }}
    >
      {children}
    </a>
  );
}
