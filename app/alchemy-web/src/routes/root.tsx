import { createRootRoute } from "@tanstack/react-router";

import { Layout } from "../components/Layout.tsx";
import { NotFound } from "./not-found.tsx";

/** Parent of every page: renders the layout, and the 404 inside it. */
export const rootRoute = createRootRoute({
  component: Layout,
  notFoundComponent: NotFound,
});
