/** Shown for a figure alchemyd could not measure. Never shown as zero. */
export const NOT_MEASURED = "–";

const UNITS = ["MiB", "GiB", "TiB", "PiB"];

/** A byte count in binary units: `512 B`, `1.5 KiB`, `3.9 GiB`. */
export function formatBytes(value: number): string {
  if (value < 1024) return `${Math.round(value)} B`;
  let scaled = value / 1024;
  let unit = "KiB";
  for (const next of UNITS) {
    if (scaled < 1024) break;
    scaled /= 1024;
    unit = next;
  }
  return `${scaled.toFixed(1)} ${unit}`;
}

/** A byte rate, or {@link NOT_MEASURED}. */
export function formatRate(value: number | null): string {
  return value === null ? NOT_MEASURED : `${formatBytes(value)}/s`;
}

/** A percentage with one decimal, or {@link NOT_MEASURED}. */
export function formatPercent(value: number | null): string {
  return value === null ? NOT_MEASURED : `${value.toFixed(1)}%`;
}

/** The sum of the measured values, or `null` when none was measured. */
export function sumMeasured(...values: (number | null)[]): number | null {
  const measured = values.filter((v): v is number => v !== null);
  return measured.length === 0 ? null : measured.reduce((a, b) => a + b, 0);
}
