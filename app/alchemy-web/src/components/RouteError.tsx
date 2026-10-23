import { useQueryErrorResetBoundary } from "@tanstack/react-query";
import { type ErrorComponentProps, useRouter } from "@tanstack/react-router";
import { useEffect } from "react";

import { ErrorState } from "./ErrorState.tsx";
import { PageHeader } from "./PageHeader.tsx";

/**
 * The router's default error component: a page that threw while rendering or
 * loading shows this inside the layout instead. "Try again" reloads the route,
 * which also resets the boundary; any query errors thrown into it are reset
 * first so they refetch.
 */
export function RouteError({ error }: ErrorComponentProps) {
  const router = useRouter();
  const queryErrors = useQueryErrorResetBoundary();

  useEffect(() => {
    queryErrors.reset();
  }, [queryErrors]);

  return (
    <>
      <PageHeader title="Something went wrong" />
      <ErrorState
        error={error}
        title="This page couldn't be shown"
        onRetry={() => void router.invalidate()}
      />
    </>
  );
}
