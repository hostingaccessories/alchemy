import { describe, expect, it } from "vite-plus/test";

import { formatBytes, formatPercent, formatRate, NOT_MEASURED, sumMeasured } from "./units.ts";

describe("units", () => {
  it.each([
    [0, "0 B"],
    [1023, "1023 B"],
    [1024, "1.0 KiB"],
    [1536, "1.5 KiB"],
    [5 * 1024 ** 2, "5.0 MiB"],
    [3 * 1024 ** 3, "3.0 GiB"],
    [2 * 1024 ** 5, "2.0 PiB"],
  ])("formats %d bytes as %s", (value, want) => {
    expect(formatBytes(value)).toBe(want);
  });

  it("formats rates and percentages, and never shows a missing figure as zero", () => {
    expect(formatRate(1536)).toBe("1.5 KiB/s");
    expect(formatRate(0)).toBe("0 B/s");
    expect(formatRate(null)).toBe(NOT_MEASURED);
    expect(formatPercent(12.345)).toBe("12.3%");
    expect(formatPercent(0)).toBe("0.0%");
    expect(formatPercent(null)).toBe(NOT_MEASURED);
  });

  it("adds only measured values", () => {
    expect(sumMeasured(1, null, 2)).toBe(3);
    expect(sumMeasured(null, null)).toBeNull();
    expect(sumMeasured(0)).toBe(0);
  });
});
