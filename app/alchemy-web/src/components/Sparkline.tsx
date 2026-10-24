/** One value at one time; `v` is `null` where nothing was measured. */
export interface SparkPoint {
  /** Unix seconds. */
  t: number;
  v: number | null;
}

/** The drawing area, in SVG user units; the SVG stretches to its box. */
const WIDTH = 100;
const HEIGHT = 30;

/** Points further apart than this many steps have a gap between them. */
const GAP_STEPS = 1.5;

/** A sparkline's runs of connected points, and the lone points between gaps. */
export interface SparkShape {
  /** SVG path data, one `M … L …` run per connected stretch. */
  d: string;
  /** Points with no neighbour to join, drawn as dots. */
  dots: { x: number; y: number }[];
}

/**
 * Lay `points` (oldest first) out from `from` to `to` (Unix seconds), scaled
 * so `max` (or the largest value) is the top. A missing value, or a jump of
 * more than {@link GAP_STEPS} steps, breaks the line rather than drawing a
 * fake zero across the gap.
 */
export function sparkShape(
  points: SparkPoint[],
  step: number,
  from: number,
  to: number,
  max?: number,
): SparkShape {
  const values = points.map((p) => p.v).filter((v): v is number => v !== null);
  const top = Math.max(max ?? 0, ...values, Number.EPSILON);
  const span = Math.max(to - from, 1);
  const x = (t: number) => round(((t - from) / span) * WIDTH);
  const y = (v: number) => round(HEIGHT - (Math.min(v, top) / top) * HEIGHT);

  const runs: { x: number; y: number }[][] = [];
  let run: { x: number; y: number }[] = [];
  let last: number | null = null;
  for (const point of points) {
    if (point.v === null) {
      runs.push(run);
      run = [];
      last = null;
      continue;
    }
    if (last !== null && point.t - last > step * GAP_STEPS) {
      runs.push(run);
      run = [];
    }
    run.push({ x: x(point.t), y: y(point.v) });
    last = point.t;
  }
  runs.push(run);

  const d = runs
    .filter((r) => r.length > 1)
    .map((r) => r.map((p, i) => `${i === 0 ? "M" : "L"}${p.x} ${p.y}`).join(" "))
    .join(" ");
  const dots = runs.flatMap((r) => (r.length === 1 ? r : []));
  return { d, dots };
}

function round(value: number): number {
  return Math.round(value * 100) / 100;
}

/**
 * A small line chart of one series over a time window, for a card or a
 * table cell. Gaps in the data stay gaps. `label` names the series and the
 * window for screen readers, such as "CPU over the last 24 hours".
 */
export function Sparkline({
  points,
  step,
  from,
  to,
  max,
  label,
  className = "h-8 w-full",
}: {
  points: SparkPoint[];
  /** Seconds between points. */
  step: number;
  /** The window's start, in Unix seconds. */
  from: number;
  /** The window's end, in Unix seconds. */
  to: number;
  /** The value at the top of the chart; the largest value if omitted. */
  max?: number;
  label: string;
  className?: string;
}) {
  const { d, dots } = sparkShape(points, step, from, to, max);
  return (
    <svg
      role="img"
      aria-label={label}
      viewBox={`0 0 ${WIDTH} ${HEIGHT}`}
      preserveAspectRatio="none"
      className={`overflow-visible text-accent ${className}`}
    >
      <line
        x1={0}
        x2={WIDTH}
        y1={HEIGHT}
        y2={HEIGHT}
        className="stroke-border"
        strokeWidth={1}
        vectorEffect="non-scaling-stroke"
      />
      {d && (
        <path
          d={d}
          fill="none"
          stroke="currentColor"
          strokeWidth={1.5}
          strokeLinejoin="round"
          strokeLinecap="round"
          vectorEffect="non-scaling-stroke"
        />
      )}
      {dots.map((dot) => (
        <line
          key={`${dot.x},${dot.y}`}
          x1={dot.x}
          x2={dot.x}
          y1={dot.y}
          y2={dot.y}
          stroke="currentColor"
          strokeWidth={3}
          strokeLinecap="round"
          vectorEffect="non-scaling-stroke"
        />
      ))}
    </svg>
  );
}
