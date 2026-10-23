import { describe, expect, it } from "vite-plus/test";

/**
 * Tailwind's palette is switched off in `style.css`, so a palette class such
 * as `text-stone-400` silently renders nothing. Catch them here.
 */
const sources = import.meta.glob<string>(["./**/*.{ts,tsx}", "!./**/*.test.{ts,tsx}"], {
  query: "?raw",
  import: "default",
  eager: true,
});

const PALETTE =
  /\b(?:bg|text|border(?:-[trblxy])?|ring|ring-offset|divide|outline|fill|stroke|from|via|to|shadow|decoration|accent|caret|placeholder)-(?:white|black|(?:slate|gray|zinc|neutral|stone|red|orange|amber|yellow|lime|green|emerald|teal|cyan|sky|blue|indigo|violet|purple|fuchsia|pink|rose|mauve|olive|mist|taupe)-\d{2,3})\b/g;

describe("theme tokens", () => {
  it("finds the sources", () => {
    expect(Object.keys(sources).length).toBeGreaterThan(5);
  });

  it.each(Object.entries(sources))("%s uses no palette colours", (_path, source) => {
    expect(source.match(PALETTE) ?? []).toEqual([]);
  });
});
