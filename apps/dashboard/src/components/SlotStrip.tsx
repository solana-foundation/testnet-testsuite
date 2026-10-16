// The slot strip: one column per confirmed slot, newest on the right.
//   status row   produced (recessive) / skipped / our failures / unavailable
//   leader lane  producing client (Agave, Firedancer, JitoLabs, other)
//   bars         non-vote transactions: others (de-emphasised) under ours
// Hover for the slot, click to open it.

import { useEffect, useMemo, useRef, useState } from "react";

import type { SlotRecord } from "../api";
import { CLIENTS, clientVar, cssVar } from "../colors";
import { compact, short } from "../format";
import { useThemeVersion, useWidth } from "../hooks";
import { KeyValues, StatusLabel, Tooltip } from "./Tooltip";

const CELL = 6;
const GAP = 2;
const PITCH = CELL + GAP;
const STATUS_H = 14;
const LANE_Y = STATUS_H + 4;
const LANE_H = 4;
const BARS_Y = LANE_Y + LANE_H + 6;
const BARS_H = 44;
const HEIGHT = BARS_Y + BARS_H;

export function slotState(
  slot: SlotRecord,
): "produced" | "skipped" | "failures" | "unavailable" {
  if (slot.status === "skipped") return "skipped";
  if (slot.status === "unavailable") return "unavailable";
  return slot.tx_ours_failed > 0 ? "failures" : "produced";
}

function stateColor(state: ReturnType<typeof slotState>): string {
  switch (state) {
    case "skipped":
      return cssVar("--warning");
    case "failures":
      return cssVar("--critical");
    case "unavailable":
      return cssVar("--text-muted");
    default:
      return cssVar("--axis");
  }
}

function roundedTop(
  ctx: CanvasRenderingContext2D,
  x: number,
  y: number,
  w: number,
  h: number,
  r: number,
) {
  const radius = Math.min(r, h, w / 2);
  ctx.beginPath();
  ctx.moveTo(x, y + h);
  ctx.lineTo(x, y + radius);
  ctx.arcTo(x, y, x + radius, y, radius);
  ctx.arcTo(x + w, y, x + w, y + radius, radius);
  ctx.lineTo(x + w, y + h);
  ctx.closePath();
  ctx.fill();
}

export function SlotStrip({
  slots,
  onSelect,
}: {
  slots: SlotRecord[];
  onSelect: (slot: SlotRecord) => void;
}) {
  const wrap = useRef<HTMLDivElement>(null);
  const canvas = useRef<HTMLCanvasElement>(null);
  const width = useWidth(wrap);
  const theme = useThemeVersion();
  const [hover, setHover] = useState<{
    slot: SlotRecord;
    x: number;
    y: number;
  } | null>(null);
  const [table, setTable] = useState(false);

  const visible = useMemo(
    () => slots.slice(-Math.max(1, Math.floor(width / PITCH))),
    [slots, width],
  );
  const right = width - (width % PITCH);
  const startX = right - visible.length * PITCH;

  useEffect(() => {
    const el = canvas.current;
    if (!el) return;
    const dpr = window.devicePixelRatio || 1;
    el.width = Math.round(width * dpr);
    el.height = Math.round(HEIGHT * dpr);
    el.style.width = `${width}px`;
    el.style.height = `${HEIGHT}px`;
    const ctx = el.getContext("2d");
    if (!ctx) return;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, width, HEIGHT);
    const max = Math.max(1, ...visible.map((s) => s.tx_total - s.tx_vote));
    const others = cssVar("--deemphasis");
    const ours = cssVar("--series-1");
    const baseline = cssVar("--axis");
    visible.forEach((slot, i) => {
      const x = startX + i * PITCH;
      const state = slotState(slot);
      ctx.fillStyle = stateColor(state);
      if (state === "unavailable") {
        ctx.strokeStyle = ctx.fillStyle;
        ctx.lineWidth = 1;
        ctx.strokeRect(x + 0.5, 0.5, CELL - 1, STATUS_H - 1);
      } else {
        roundedTop(ctx, x, 0, CELL, STATUS_H, 2);
      }
      if (slot.leader_client) {
        ctx.fillStyle = cssVar(clientVar(slot.leader_client));
        ctx.fillRect(x, LANE_Y, CELL, LANE_H);
      }
      const nonVote = slot.tx_total - slot.tx_vote;
      const theirs = Math.max(0, nonVote - slot.tx_ours);
      const hTheirs = (theirs / max) * BARS_H;
      const hOurs = (slot.tx_ours / max) * BARS_H;
      const bottom = BARS_Y + BARS_H;
      if (hTheirs > 0) {
        ctx.fillStyle = others;
        roundedTop(ctx, x, bottom - hTheirs, CELL, hTheirs, hOurs > 0 ? 0 : 2);
      }
      if (hOurs > 0) {
        // 2px surface gap between the stacked segments.
        const gap = hTheirs > 0 ? GAP : 0;
        ctx.fillStyle = ours;
        roundedTop(
          ctx,
          x,
          bottom - hTheirs - gap - Math.max(hOurs, 1),
          CELL,
          Math.max(hOurs, 1),
          2,
        );
      }
    });
    ctx.fillStyle = baseline;
    ctx.fillRect(0, BARS_Y + BARS_H - 0.5, width, 1);
  }, [visible, width, startX, theme]);

  const pick = (clientX: number, rect: DOMRect): SlotRecord | null => {
    const i = Math.floor((clientX - rect.left - startX) / PITCH);
    return visible[i] ?? null;
  };

  const first = visible[0];
  const last = visible.at(-1);
  return (
    <section className="card">
      <div className="card-head">
        <h2>Slots</h2>
        <span className="sub">
          {first && last
            ? `${first.slot.toLocaleString()} – ${last.slot.toLocaleString()} · confirmed, newest right`
            : "waiting for chain-watch"}
        </span>
        <span className="spacer" />
        <button aria-pressed={table} onClick={() => setTable(!table)}>
          Table
        </button>
      </div>
      <div ref={wrap} style={{ position: "relative" }}>
        <canvas
          ref={canvas}
          role="img"
          aria-label="Recent slots: status, leader client and transactions per slot"
          style={{ cursor: "pointer", display: "block" }}
          onMouseMove={(e) => {
            const slot = pick(
              e.clientX,
              e.currentTarget.getBoundingClientRect(),
            );
            setHover(slot ? { slot, x: e.clientX, y: e.clientY } : null);
          }}
          onMouseLeave={() => setHover(null)}
          onClick={(e) => {
            const slot = pick(
              e.clientX,
              e.currentTarget.getBoundingClientRect(),
            );
            if (slot) onSelect(slot);
          }}
        />
      </div>
      <div className="legend">
        <StatusLabel color="var(--axis)" label="produced" />
        <StatusLabel color="var(--warning)" label="skipped" />
        <StatusLabel color="var(--critical)" label="our tx failed" />
        <StatusLabel color="var(--text-muted)" label="unavailable" hollow />
        <span style={{ width: 12 }} />
        {CLIENTS.map((c) => (
          <span key={c}>
            <span
              className="key-rect"
              style={{ background: `var(${clientVar(c)})`, height: 4 }}
            />
            {c}
          </span>
        ))}
        <span>
          <span
            className="key-rect"
            style={{ background: `var(${clientVar(null)})`, height: 4 }}
          />
          other client
        </span>
        <span style={{ width: 12 }} />
        <span>
          <span
            className="key-rect"
            style={{ background: "var(--deemphasis)" }}
          />
          non-vote tx
        </span>
        <span>
          <span
            className="key-rect"
            style={{ background: "var(--series-1)" }}
          />
          ours
        </span>
      </div>
      {hover && (
        <Tooltip x={hover.x} y={hover.y}>
          <div className="title">slot {hover.slot.slot.toLocaleString()}</div>
          <KeyValues
            rows={[
              ["status", slotState(hover.slot)],
              ["leader", hover.slot.leader ? short(hover.slot.leader) : "–"],
              [
                "client",
                `${hover.slot.leader_client ?? "–"} ${hover.slot.leader_version ?? ""}`,
              ],
              [
                "non-vote tx",
                compact(hover.slot.tx_total - hover.slot.tx_vote),
              ],
              [
                "ours",
                `${hover.slot.tx_ours} (${hover.slot.tx_ours_failed} failed)`,
              ],
              ["failed (all)", compact(hover.slot.tx_failed)],
              ["CU", compact(hover.slot.cu_consumed)],
              [
                "prio p50 / p90",
                `${compact(hover.slot.prio_p50)} / ${compact(hover.slot.prio_p90)}`,
              ],
            ]}
          />
        </Tooltip>
      )}
      {table && (
        <SlotTable slots={visible.slice(-60).reverse()} onSelect={onSelect} />
      )}
    </section>
  );
}

function SlotTable({
  slots,
  onSelect,
}: {
  slots: SlotRecord[];
  onSelect: (slot: SlotRecord) => void;
}) {
  return (
    <div className="scroll" style={{ marginTop: 10 }}>
      <table>
        <thead>
          <tr>
            <th className="num">slot</th>
            <th>status</th>
            <th>client</th>
            <th className="num">non-vote</th>
            <th className="num">ours</th>
            <th className="num">ours failed</th>
            <th className="num">CU</th>
            <th className="num">prio p50</th>
          </tr>
        </thead>
        <tbody>
          {slots.map((s) => (
            <tr key={s.slot} className="clickable" onClick={() => onSelect(s)}>
              <td className="num">{s.slot}</td>
              <td>{slotState(s)}</td>
              <td>{s.leader_client ?? "–"}</td>
              <td className="num">{s.tx_total - s.tx_vote}</td>
              <td className="num">{s.tx_ours}</td>
              <td className="num">{s.tx_ours_failed}</td>
              <td className="num">{compact(s.cu_consumed)}</td>
              <td className="num">{compact(s.prio_p50)}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
