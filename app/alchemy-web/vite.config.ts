import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite-plus";

/** Dev-server proxy target. Mirrors `alchemyd::DEFAULT_LISTEN`. */
const daemon = "http://127.0.0.1:7240";

export default defineConfig({
  plugins: [react(), tailwindcss()],
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
    proxy: {
      /**
       * Keep the browser's `Host` (`localhost:5173`) rather than the
       * shorthand's rewrite to the daemon's, so it still matches `Origin`
       * and alchemyd treats mutating requests as same-origin.
       */
      "/api": { target: daemon, changeOrigin: false },
    },
  },
  test: {
    environment: "jsdom",
    include: ["src/**/*.test.{ts,tsx}"],
  },
});
