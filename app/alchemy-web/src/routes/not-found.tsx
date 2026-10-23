import { Link } from "@tanstack/react-router";

import { PageHeader } from "../components/PageHeader.tsx";

/** Shown inside the layout for any path no route matches. */
export function NotFound() {
  return (
    <div className="flex flex-col gap-2">
      <PageHeader title="Not found" />
      <p className="text-muted">There's nothing at this address.</p>
      <p>
        <Link to="/apps" className="text-accent underline">
          Go to Apps
        </Link>
      </p>
    </div>
  );
}
