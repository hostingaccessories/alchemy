import tailwindcss from "@tailwindcss/vite";
import { defineConfig } from "astro/config";

export default defineConfig({
  site: "https://alchemy.computer",
  output: "static",
  vite: {
    plugins: [tailwindcss()],
  },
});
