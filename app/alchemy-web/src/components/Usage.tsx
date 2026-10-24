import { Link } from "@tanstack/react-router";
import type { ReactNode } from "react";

import type {
  MachineUsage,
  MetricsHistory,
  MetricsPoint,
  MetricsRange,
  MetricsResponse,
} from "../lib/api.ts";
import { NOT_MEASURED, formatBytes, formatPercent, formatRate, sumMeasured } from "../lib/units.ts";
import { type SparkPoint, Sparkline } from "./Sparkline.tsx";

/** Every history range: how a chart's label says it, and how long it is. */
const RANGE_INFO: Record<MetricsRange, { label: string; seconds: number }> = {
  "1h": { label: "last hour", seconds: 3600 },
  "12h": { label: "last 12 hours", seconds: 12 * 3600 },
  "24h": { label: "last 24 hours", seconds: 24 * 3600 },
  "7d": { label: "last 7 days", seconds: 7 * 86400 },
  "30d": { label: "last 30 days", seconds: 30 * 86400 },
};

/** Every history range, shortest first. */
export const RANGES = Object.keys(RANGE_INFO) as MetricsRange[];

/** The chart window for `range` ending at `now`, in Unix seconds. */
export function rangeWindow(range: MetricsRange, now: number) {
  const { label, seconds } = RANGE_INFO[range];
  return { from: now - seconds, to: now, label };
}

/** One value per stored point, `null` where it was not measured. */
export function series(
  history: MetricsHistory | undefined,
  pick: (point: MetricsPoint) => number | null | undefined,
): SparkPoint[] {
  return (history?.points ?? []).map((point) => ({ t: point.t, v: pick(point) ?? null }));
}

/** The chart for one figure, or nothing yet to chart. */
interface Chart {
  points: SparkPoint[];
  step: number;
  from: number;
  to: number;
  label: string;
  max?: number;
}

function Card({
  title,
  value,
  detail,
  chart,
}: {
  title: string;
  value: string;
  detail?: ReactNode;
  chart?: Chart;
}) {
  return (
    <section
      aria-label={title}
      className="flex flex-col gap-1 rounded-lg border border-border bg-surface-raised p-3"
    >
      <h3 className="text-sm text-muted">{title}</h3>
      <p className="text-lg font-semibold tabular-nums">{value}</p>
      {detail && <p className="text-sm text-muted tabular-nums">{detail}</p>}
      {chart && <Sparkline {...chart} className="mt-1 h-8 w-full" />}
    </section>
  );
}

function of(used: number | null, total: number | null): string {
  if (used === null || total === null) return NOT_MEASURED;
  return `${formatBytes(used)} of ${formatBytes(total)}`;
}

/**
 * This machine's CPU, memory, disk, and network: the latest sample, with
 * each figure charted over the chosen range.
 */
export function MachineUsageView({
  metrics,
  history,
  range,
  now,
}: {
  metrics: MetricsResponse;
  history: MetricsHistory | undefined;
  range: MetricsRange;
  now: number;
}) {
  const machine: MachineUsage | null = metrics.machine;
  if (!machine) {
    return <p className="text-muted">Alchemy couldn't read this machine's usage.</p>;
  }
  const { from, to, label } = rangeWindow(range, now);
  const step = history?.step ?? metrics.interval;
  const chart = (name: string, pick: (m: MachineUsage) => number | null, max?: number): Chart => ({
    points: series(history, (p) => (p.machine ? pick(p.machine) : null)),
    step,
    from,
    to,
    label: `${name} over the ${label}`,
    max,
  });
  const load = machine.load?.map((l) => l.toFixed(2)).join(" ");
  return (
    <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
      <Card
        title="CPU"
        value={formatPercent(machine.cpu)}
        detail={
          <>
            {machine.cpus} {machine.cpus === 1 ? "CPU" : "CPUs"}
            {load && ` · load ${load}`}
          </>
        }
        chart={chart("CPU", (m) => m.cpu, 100)}
      />
      <Card
        title="Memory"
        value={of(machine.memory_used, machine.memory_total)}
        detail={`Swap ${of(machine.swap_used, machine.swap_total)}`}
        chart={chart("Memory", (m) => m.memory_used, machine.memory_total)}
      />
      <Card
        title="Disk"
        value={of(machine.disk_used, machine.disk_total)}
        detail={`Read ${formatRate(machine.disk_read)} · write ${formatRate(machine.disk_write)}`}
        chart={chart("Disk activity", (m) => sumMeasured(m.disk_read, m.disk_write))}
      />
      <Card
        title="Network"
        value={`In ${formatRate(machine.net_rx)}`}
        detail={`Out ${formatRate(machine.net_tx)}`}
        chart={chart("Network traffic", (m) => sumMeasured(m.net_rx, m.net_tx))}
      />
    </div>
  );
}

/**
 * Each running app's latest CPU and memory, with its CPU charted over the
 * chosen range. A row is a Compose project; one that `appFor` names an app
 * for links to that app's page.
 */
export function AppUsageTable({
  metrics,
  history,
  range,
  now,
  appFor = () => undefined,
}: {
  metrics: MetricsResponse;
  history: MetricsHistory | undefined;
  range: MetricsRange;
  now: number;
  appFor?: (project: string) => string | undefined;
}) {
  if (metrics.projects.length === 0) {
    return <p className="text-muted">No apps are running.</p>;
  }
  const { from, to, label } = rangeWindow(range, now);
  const step = history?.step ?? metrics.interval;
  return (
    <table className="w-full text-sm">
      <thead className="text-left text-muted">
        <tr>
          <th className="py-1 font-normal">App</th>
          <th className="py-1 font-normal">CPU</th>
          <th className="hidden w-1/3 py-1 font-normal sm:table-cell">
            <span className="sr-only">CPU over the {label}</span>
          </th>
          <th className="py-1 font-normal">Memory</th>
          <th className="py-1 font-normal">Network</th>
        </tr>
      </thead>
      <tbody>
        {metrics.projects.map((app) => (
          <tr key={app.name} className="border-t border-border">
            <td className="py-1.5 pr-3 font-medium">
              <AppName project={app.name} app={appFor(app.name)} />
              <span className="ml-2 text-muted">
                {app.containers} {app.containers === 1 ? "container" : "containers"}
              </span>
            </td>
            <td className="py-1.5 pr-3 tabular-nums">{formatPercent(app.cpu)}</td>
            <td className="hidden py-1.5 pr-3 sm:table-cell">
              <Sparkline
                points={series(history, (p) => p.projects.find((x) => x.name === app.name)?.cpu)}
                step={step}
                from={from}
                to={to}
                label={`${app.name} CPU over the ${label}`}
                className="h-5 w-full"
              />
            </td>
            <td className="py-1.5 pr-3 tabular-nums">{formatBytes(app.memory)}</td>
            <td className="py-1.5 tabular-nums">
              {formatRate(sumMeasured(app.net_rx, app.net_tx))}
            </td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

/** A project's name, linking to its app's page when it has one. */
function AppName({ project, app }: { project: string; app: string | undefined }) {
  if (app === undefined) {
    return <>{project}</>;
  }
  return (
    <Link to="/apps/$name" params={{ name: app }} className="text-accent hover:underline">
      {project}
    </Link>
  );
}

/** Buttons to pick a history range; the current one is pressed. */
export function RangePicker({
  range,
  onChange,
}: {
  range: MetricsRange;
  onChange: (range: MetricsRange) => void;
}) {
  return (
    <div role="group" aria-label="History range" className="flex gap-1">
      {RANGES.map((r) => (
        <button
          key={r}
          type="button"
          aria-pressed={r === range}
          onClick={() => onChange(r)}
          className="rounded-md px-2 py-0.5 text-sm text-muted hover:bg-surface-muted focus-visible:ring-2 focus-visible:ring-accent focus-visible:outline-none aria-pressed:bg-surface-muted aria-pressed:text-text"
        >
          {r}
        </button>
      ))}
    </div>
  );
}
