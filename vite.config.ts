import { defineConfig } from "vite-plus";

const generated = ["**/dist/**", "**/.astro/**", "target/**", "app/alchemy-gui/gen/**"];

export default defineConfig({
  fmt: {
    // TOML is owned by the Rust toolchain; .claude/ is vendored agent config.
    ignorePatterns: [...generated, "**/*.toml", ".claude/**"],
  },
  lint: {
    ignorePatterns: generated,
    jsPlugins: [{ name: "vite-plus", specifier: "vite-plus/oxlint-plugin" }],
    rules: { "vite-plus/prefer-vite-plus-imports": "error" },
    options: { typeAware: true, typeCheck: true },
  },
  run: {
    cache: true,
  },
});
