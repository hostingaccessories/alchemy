import { QueryClient } from "@tanstack/react-query";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import { App } from "./App.tsx";
import { createAppRouter } from "./router.ts";
import "./style.css";

const root = document.getElementById("root");
if (!root) {
  throw new Error("missing #root element");
}

const router = createAppRouter();
const queryClient = new QueryClient();

createRoot(root).render(
  <StrictMode>
    <App router={router} queryClient={queryClient} />
  </StrictMode>,
);
