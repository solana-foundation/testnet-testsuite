// A uPlot time-series card: 2px lines on one y-axis, hairline grid, event
// hairlines from the timeline, a crosshair tooltip listing every series,
// a legend with each series' latest value, and a table view.

import { useEffect, useMemo, useRef, useState } from "react";
import uPlot from "uplot";
import "uplot/dist/uPlot.min.css";

import { cssVar } from "../colors";
import { clock, formatUnit, type Unit } from "../format";
import { useThemeVersion, useWidth } from "../hooks";
import type { TimelineEvent } from "../live";

/** Shared by every chart and the event lane so plot areas line up. */
export const Y_AXIS_SIZE = 64;
export const SYNC_KEY = "mtm";

export interface LineSeries {
  name: string;
  /** CSS variable name (`--series-1`), resolved when drawing. */
  color: string;
  times: number[];
  values: number[];
}

interface Props {
  title: string;
  sub?: string;
  unit: Unit;
  series: LineSeries[];
  range: [number, number];
  events: TimelineEvent[];
  loading: boolean;
  error?: string | null;
  height?: number;
  /** Show the legend even for one series (charts grouped "by" an entity). */
  grouped?: boolean;
}

/** Union of timestamps; each series aligned to it (null where missing). */
function align(series: LineSeries[]): uPlot.AlignedData {
  const times = [...new Set(series.flatMap((s) => s.times))].sort(
    (a, b) => a - b,
  );
  const columns = series.map((s) => {
    const byTime = new Map(s.times.map((t, i) => [t, s.values[i] ?? null]));
    return times.map((t) => {
      const v = byTime.get(t);
      return v == null || !Number.isFinite(v) ? null : v;
    });
  });
  return [times, ...columns] as uPlot.AlignedData;
}

export function eventLinesPlugin(events: () => TimelineEvent[]): uPlot.Plugin {
  return {
    hooks: {
      draw: (u) => {
        const ctx = u.ctx;
        const [min, max] = [u.scales.x?.min ?? 0, u.scales.x?.max ?? 0];
        ctx.save();
        ctx.strokeStyle = cssVar("--text-muted");
        ctx.globalAlpha = 0.45;
        ctx.lineWidth = 1 * devicePixelRatio;
        for (const { event } of events()) {
          const t = event.time_us / 1e6;
          if (t < min || t > max) continue;
          const x = Math.round(u.valToPos(t, "x", true)) + 0.5;
          ctx.beginPath();
          ctx.moveTo(x, u.bbox.top);
          ctx.lineTo(x, u.bbox.top + u.bbox.height);
          ctx.stroke();
        }
        ctx.restore();
      },
    },
  };
}

/** One-line time ticks (HH:MM, with the date when the span crosses days). */
export function timeAxis(): uPlot.Axis {
  return {
    ...axisStyle(),
    size: 28,
    values: (u, splits) => {
      const span = (u.scales.x?.max ?? 0) - (u.scales.x?.min ?? 0);
      return splits.map((t) =>
        new Date(t * 1000).toLocaleString([], {
          hour12: false,
          hour: "2-digit",
          minute: "2-digit",
          ...(span > 86_400 ? { month: "short", day: "numeric" } : {}),
        }),
      );
    },
  };
}

export function axisStyle(): Pick<
  uPlot.Axis,
  "stroke" | "grid" | "ticks" | "font"
> {
  return {
    stroke: cssVar("--text-muted"),
    font: "11px system-ui, sans-serif",
    grid: { stroke: cssVar("--grid"), width: 1 },
    ticks: { stroke: cssVar("--axis"), width: 1, size: 4 },
  };
}

export function Chart({
  title,
  sub,
  unit,
  series,
  range,
  events,
  loading,
  error,
  height = 200,
  grouped = false,
}: Props) {
  const wrap = useRef<HTMLDivElement>(null);
  const plot = useRef<HTMLDivElement>(null);
  const tip = useRef<HTMLDivElement>(null);
  const width = useWidth(wrap);
  const theme = useThemeVersion();
  const [table, setTable] = useState(false);
  const eventsRef = useRef(events);
  eventsRef.current = events;
  const data = useMemo(() => align(series), [series]);

  useEffect(() => {
    const el = plot.current;
    if (!el || series.length === 0) return;
    const opts: uPlot.Options = {
      width,
      height,
      legend: { show: false },
      cursor: {
        sync: { key: SYNC_KEY },
        points: { size: 8, width: 2, stroke: cssVar("--surface-1") },
        drag: { x: false, y: false },
      },
      scales: {
        x: { time: true, range: () => range },
        y: { range: (_u, _min, max) => [0, max > 0 ? max * 1.1 : 1] },
      },
      axes: [
        timeAxis(),
        {
          ...axisStyle(),
          size: Y_AXIS_SIZE,
          values: (_u, vals) => vals.map((v) => formatUnit(v, unit)),
        },
      ],
      series: [
        {},
        ...series.map((s) => ({
          label: s.name,
          stroke: cssVar(s.color),
          width: 2,
          spanGaps: false,
          points: { show: false },
        })),
      ],
      plugins: [eventLinesPlugin(() => eventsRef.current)],
      hooks: {
        setCursor: [
          (u) => {
            const box = tip.current;
            const idx = u.cursor.idx;
            if (!box) return;
            if (idx == null || u.cursor.left == null || u.cursor.left < 0) {
              box.style.display = "none";
              return;
            }
            // Untrusted labels: build with textContent only.
            box.replaceChildren();
            const head = document.createElement("div");
            head.className = "title";
            head.textContent = clock((u.data[0][idx] ?? 0) * 1e6);
            box.append(head);
            series.forEach((s, i) => {
              const row = document.createElement("div");
              row.className = "row";
              const key = document.createElement("span");
              key.className = "key-line";
              key.style.background = `var(${s.color})`;
              const value = document.createElement("b");
              value.textContent = formatUnit(
                u.data[i + 1]?.[idx] ?? null,
                unit,
              );
              const name = document.createElement("span");
              name.className = "name";
              name.textContent = s.name;
              row.append(key, value, name);
              box.append(row);
            });
            const rect = u.over.getBoundingClientRect();
            const x = rect.left + u.cursor.left;
            const y = rect.top + (u.cursor.top ?? 0);
            box.style.display = "block";
            box.style.left = `${x + 200 > window.innerWidth ? x - 200 : x + 14}px`;
            box.style.top = `${y + 14}px`;
          },
        ],
      },
    };
    const chart = new uPlot(opts, data, el);
    return () => chart.destroy();
  }, [data, width, height, unit, range, series, theme]);

  const latest = series.map((s) => s.values.at(-1));
  return (
    <section className="card">
      <div className="card-head">
        <h2>{title}</h2>
        <span className="spacer" />
        <button aria-pressed={table} onClick={() => setTable(!table)}>
          Table
        </button>
      </div>
      {sub && <p className="sub">{sub}</p>}
      <div ref={wrap} className={`chart${loading ? " loading" : ""}`}>
        {series.length === 0 ? (
          <div className="empty" style={{ height }}>
            {error
              ? `query failed: ${error}`
              : loading
                ? "loading…"
                : "no data in this range"}
          </div>
        ) : (
          <div ref={plot} />
        )}
        <div ref={tip} className="tooltip" style={{ display: "none" }} />
      </div>
      {(series.length > 1 || (grouped && series.length > 0)) && (
        <div className="legend">
          {series.map((s, i) => (
            <span key={s.name}>
              <span
                className="key-line"
                style={{ background: `var(${s.color})` }}
              />
              {s.name}
              <b style={{ color: "var(--text-primary)" }}>
                {formatUnit(latest[i] ?? null, unit)}
              </b>
            </span>
          ))}
        </div>
      )}
      {table && <SeriesTable series={series} unit={unit} />}
    </section>
  );
}

function SeriesTable({ series, unit }: { series: LineSeries[]; unit: Unit }) {
  const [times, ...columns] = align(series) as [
    number[],
    ...(number | null)[][],
  ];
  const rows = times
    .map((t, i) => ({ t, values: columns.map((c) => c[i] ?? null) }))
    .slice(-30)
    .reverse();
  return (
    <div className="scroll" style={{ marginTop: 8, maxHeight: 260 }}>
      <table>
        <thead>
          <tr>
            <th>time</th>
            {series.map((s) => (
              <th key={s.name} className="num">
                {s.name}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((row) => (
            <tr key={row.t}>
              <td>{clock(row.t * 1e6)}</td>
              {row.values.map((v, i) => (
                <td key={series[i]?.name ?? i} className="num">
                  {formatUnit(v, unit)}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
