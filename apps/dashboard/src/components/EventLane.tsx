// The event timeline: one lane per kind on the charts' x-axis (same y-axis
// gutter, same cursor sync). Identity is carried by shape and row, inked in
// text tokens, so it never depends on colour. Hover for details; the table
// lists every event in range.

import { useEffect, useMemo, useRef, useState } from "react";
import uPlot from "uplot";

import { cssVar } from "../colors";
import { dateTime, short } from "../format";
import { useThemeVersion, useWidth } from "../hooks";
import type { TimelineEvent } from "../live";
import { SYNC_KEY, Y_AXIS_SIZE, axisStyle, timeAxis } from "./Chart";
import { KeyValues, Tooltip } from "./Tooltip";

type Shape =
  "tick" | "diamond" | "hollow" | "triangle" | "dot" | "square" | "cross";

// Short: these sit in the charts' y-axis gutter.
const ROWS = ["epoch", "gates", "version", "runs"] as const;
const ROW_H = 22;
const HEIGHT = ROWS.length * ROW_H + 30;

function placement(e: TimelineEvent): { row: number; shape: Shape } {
  switch (e.event.kind) {
    case "epoch":
      return { row: 0, shape: "tick" };
    case "feature_activated":
      return { row: 1, shape: "diamond" };
    case "feature_pending":
      return { row: 1, shape: "hollow" };
    case "majority_version":
      return { row: 2, shape: "triangle" };
    case "gap":
      return { row: 2, shape: "cross" };
    case "job_started":
      return { row: 3, shape: "dot" };
    case "job_finished":
      return { row: 3, shape: "square" };
    case "watch_started":
      return { row: 3, shape: "hollow" };
    default:
      return { row: 2, shape: "dot" };
  }
}

export function describe(e: TimelineEvent): string {
  const d = e.event.detail ?? {};
  const text = (k: string) =>
    typeof d[k] === "string" ? (d[k] as string) : "";
  switch (e.event.kind) {
    case "epoch":
      return `epoch ${e.event.subject} begins`;
    case "feature_activated":
      return `feature activated: ${text("description") || short(e.event.subject)}`;
    case "feature_pending":
      return `feature pending (next epoch): ${text("description") || short(e.event.subject)}`;
    case "majority_version":
      return `stake majority now ${e.event.subject} (was ${text("previous")})`;
    case "gap":
      return `chain-watch skipped slots ${e.event.subject}`;
    case "job_started":
      return `job started: ${e.event.subject}`;
    case "job_finished":
      return `job ${text("state") || "finished"}: ${e.event.subject}`;
    case "watch_started":
      return `chain-watch run ${e.event.subject} started`;
    default:
      return `${e.event.kind}: ${e.event.subject}`;
  }
}

function drawShape(
  ctx: CanvasRenderingContext2D,
  shape: Shape,
  x: number,
  y: number,
  r: number,
) {
  ctx.beginPath();
  switch (shape) {
    case "tick":
      ctx.fillRect(
        x - 0.5 * devicePixelRatio,
        y - r * 1.4,
        devicePixelRatio,
        r * 2.8,
      );
      return;
    case "diamond":
    case "hollow":
      ctx.moveTo(x, y - r);
      ctx.lineTo(x + r, y);
      ctx.lineTo(x, y + r);
      ctx.lineTo(x - r, y);
      ctx.closePath();
      if (shape === "diamond") ctx.fill();
      else ctx.stroke();
      return;
    case "triangle":
      ctx.moveTo(x, y - r);
      ctx.lineTo(x + r, y + r);
      ctx.lineTo(x - r, y + r);
      ctx.closePath();
      ctx.fill();
      return;
    case "square":
      ctx.fillRect(x - r * 0.8, y - r * 0.8, r * 1.6, r * 1.6);
      return;
    case "cross":
      ctx.moveTo(x - r, y - r);
      ctx.lineTo(x + r, y + r);
      ctx.moveTo(x + r, y - r);
      ctx.lineTo(x - r, y + r);
      ctx.stroke();
      return;
    default:
      ctx.arc(x, y, r * 0.8, 0, Math.PI * 2);
      ctx.fill();
  }
}

export function EventLane({
  events,
  range,
}: {
  events: TimelineEvent[];
  range: [number, number];
}) {
  const wrap = useRef<HTMLDivElement>(null);
  const plot = useRef<HTMLDivElement>(null);
  const width = useWidth(wrap);
  const theme = useThemeVersion();
  const [hover, setHover] = useState<{
    events: TimelineEvent[];
    x: number;
    y: number;
  } | null>(null);
  const [table, setTable] = useState(false);
  const inRange = useMemo(
    () =>
      events.filter(
        (e) =>
          e.event.time_us / 1e6 >= range[0] &&
          e.event.time_us / 1e6 <= range[1],
      ),
    [events, range],
  );
  const eventsRef = useRef(inRange);
  eventsRef.current = inRange;

  useEffect(() => {
    const el = plot.current;
    if (!el) return;
    const opts: uPlot.Options = {
      width,
      height: HEIGHT,
      legend: { show: false },
      cursor: {
        sync: { key: SYNC_KEY },
        points: { show: false },
        drag: { x: false, y: false },
      },
      scales: {
        x: { time: true, range: () => range },
        y: { range: [0, ROWS.length] },
      },
      axes: [
        timeAxis(),
        {
          ...axisStyle(),
          size: Y_AXIS_SIZE,
          grid: { show: false },
          ticks: { show: false },
          splits: () => ROWS.map((_, i) => ROWS.length - i - 0.5),
          values: () => [...ROWS],
        },
      ],
      series: [{}, { show: false }],
      hooks: {
        draw: [
          (u) => {
            const ctx = u.ctx;
            const r = 5 * devicePixelRatio;
            ctx.save();
            ctx.fillStyle = cssVar("--text-primary");
            ctx.strokeStyle = cssVar("--text-primary");
            ctx.lineWidth = 1.5 * devicePixelRatio;
            for (const e of eventsRef.current) {
              const { row, shape } = placement(e);
              const x = u.valToPos(e.event.time_us / 1e6, "x", true);
              const y = u.valToPos(ROWS.length - row - 0.5, "y", true);
              drawShape(ctx, shape, x, y, r);
            }
            ctx.restore();
          },
        ],
      },
    };
    const chart = new uPlot(
      opts,
      [
        [range[0], range[1]],
        [null, null],
      ],
      el,
    );
    const over = chart.over;
    const move = (ev: MouseEvent) => {
      const rect = over.getBoundingClientRect();
      const px = ev.clientX - rect.left;
      const row = Math.floor(
        ((ev.clientY - rect.top) / rect.height) * ROWS.length,
      );
      const near = eventsRef.current.filter(
        (e) =>
          placement(e).row === row &&
          Math.abs(chart.valToPos(e.event.time_us / 1e6, "x") - px) <= 8,
      );
      setHover(
        near.length ? { events: near, x: ev.clientX, y: ev.clientY } : null,
      );
    };
    const leave = () => setHover(null);
    over.addEventListener("mousemove", move);
    over.addEventListener("mouseleave", leave);
    return () => {
      over.removeEventListener("mousemove", move);
      over.removeEventListener("mouseleave", leave);
      chart.destroy();
    };
  }, [width, range, theme, inRange]);

  return (
    <section className="card">
      <div className="card-head">
        <h2>Timeline</h2>
        <span className="sub">
          ◆ feature activated · ◇ pending · │ epoch · ▲ majority version · ✕ gap
          · ● job started · ■ job finished · ◇ (runs) watch started
        </span>
        <span className="spacer" />
        <button aria-pressed={table} onClick={() => setTable(!table)}>
          Table
        </button>
      </div>
      <div ref={wrap}>
        <div ref={plot} />
      </div>
      {hover && (
        <Tooltip x={hover.x} y={hover.y}>
          {hover.events.slice(0, 6).map((e) => (
            <div key={`${e.source}-${e.event.id}`} style={{ marginBottom: 4 }}>
              <div className="title">{dateTime(e.event.time_us)}</div>
              <KeyValues
                rows={[
                  ["event", describe(e)],
                  ...(e.event.slot != null
                    ? ([["slot", e.event.slot.toLocaleString()]] as [
                        string,
                        string,
                      ][])
                    : []),
                ]}
              />
            </div>
          ))}
        </Tooltip>
      )}
      {table && (
        <div className="scroll" style={{ marginTop: 8, maxHeight: 260 }}>
          <table>
            <thead>
              <tr>
                <th>time</th>
                <th>source</th>
                <th>event</th>
                <th className="num">slot</th>
              </tr>
            </thead>
            <tbody>
              {[...inRange].reverse().map((e) => (
                <tr key={`${e.source}-${e.event.id}`}>
                  <td>{dateTime(e.event.time_us)}</td>
                  <td>{e.source}</td>
                  <td>{describe(e)}</td>
                  <td className="num">{e.event.slot ?? "–"}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}
