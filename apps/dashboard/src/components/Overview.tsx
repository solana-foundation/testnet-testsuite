// Observability: time range (one row, scopes everything below), headline
// tiles, the live slot strip, the event timeline and the charts that share
// its x-axis, then the failure breakdown.

import { useEffect, useMemo, useState } from "react";

import { queryInstant, queryRange, type SlotRecord } from "../api";
import { CLIENTS, OUTCOMES, PROGRAMS, entityVar } from "../colors";
import { compact, percent, seconds, type Unit } from "../format";
import { usePoll } from "../hooks";
import type { LiveState } from "../live";
import { Chart, type LineSeries } from "./Chart";
import { EventLane } from "./EventLane";
import { SlotStrip } from "./SlotStrip";

const RANGES: [string, number][] = [
  ["15 min", 900],
  ["1 h", 3_600],
  ["6 h", 21_600],
  ["24 h", 86_400],
];

interface ChartSpec {
  title: string;
  sub: string;
  unit: Unit;
  /** Each query's series are named by `label(metric labels)`. */
  queries: {
    query: string;
    label: (labels: Record<string, string>) => string;
  }[];
  /** Fixed entity order: colour follows the entity, never its rank. */
  order: readonly string[];
  /** Series are entities of one dimension: keep the legend even for one. */
  grouped?: boolean;
}

const fixed = (name: string) => () => name;

const CHARTS: ChartSpec[] = [
  {
    title: "Transactions per second",
    sub: "from every confirmed block; the node's 60 s performance samples when blocks are not fetched (public RPC)",
    unit: "per_s",
    order: ["cluster non-vote", "ours"],
    queries: [
      {
        query:
          'sum(rate(chain_block_transactions_total{kind="non_vote"}[1m])) or sum(chain_perf_tps{kind="non_vote"})',
        label: fixed("cluster non-vote"),
      },
      {
        query: "sum(rate(chain_ours_transactions_total[1m]))",
        label: fixed("ours"),
      },
    ],
  },
  {
    title: "Our transactions by outcome",
    grouped: true,
    sub: "per second, as each service saw them (mtm_chain::tag)",
    unit: "per_s",
    order: OUTCOMES,
    queries: [
      {
        query: "sum by (outcome) (rate(chain_tx_outcome_total[1m]))",
        label: (l) => l.outcome ?? "?",
      },
    ],
  },
  {
    title: "Settle time, landed transactions",
    sub: "send → commitment; averaged across services",
    unit: "seconds",
    order: ["p50", "p95"],
    queries: [
      {
        query: 'avg(chain_tx_settle_seconds{quantile="0.5",outcome="landed"})',
        label: fixed("p50"),
      },
      {
        query: 'avg(chain_tx_settle_seconds{quantile="0.95",outcome="landed"})',
        label: fixed("p95"),
      },
    ],
  },
  {
    title: "Skip rate by leader client",
    grouped: true,
    sub: "share of each client's leader slots with no block (5 min)",
    unit: "percent",
    order: CLIENTS,
    queries: [
      {
        query:
          '(sum by (leader_client) (rate(chain_slots_total{status="skipped"}[5m])) or sum by (leader_client) (rate(chain_slots_total[5m])) * 0) / sum by (leader_client) (rate(chain_slots_total[5m]))',
        label: (l) => l.leader_client ?? "?",
      },
    ],
  },
  {
    title: "Compute per transaction, by program (p50)",
    grouped: true,
    sub: "our landed transactions; a step after a feature activation is a repricing",
    unit: "count",
    order: PROGRAMS,
    queries: [
      {
        query: 'max by (program) (chain_ours_cu{quantile="0.5"})',
        label: (l) => l.program ?? "?",
      },
    ],
  },
  {
    title: "Priority price (p50, micro-lamports / CU)",
    grouped: true,
    sub: "every non-vote transaction vs ours",
    unit: "count",
    order: ["all", "ours"],
    queries: [
      {
        query:
          'max by (scope) (chain_prio_price_micro_lamports{quantile="0.5"})',
        label: (l) => l.scope ?? "?",
      },
    ],
  },
];

async function loadChart(
  spec: ChartSpec,
  range: [number, number],
  step: number,
): Promise<LineSeries[]> {
  const results = await Promise.all(
    spec.queries.map((q) => queryRange(q.query, range[0], range[1], step)),
  );
  const series = results.flatMap((list, i) =>
    list.map((s) => {
      const name = spec.queries[i]?.label(s.labels) ?? "?";
      return {
        name,
        color: entityVar(name, spec.order),
        times: s.times,
        values: s.values,
      };
    }),
  );
  // Legend and tooltip in the entity order, unknowns last.
  const rank = (name: string) => {
    const i = spec.order.indexOf(name);
    return i < 0 ? spec.order.length : i;
  };
  return series.sort(
    (a, b) => rank(a.name) - rank(b.name) || a.name.localeCompare(b.name),
  );
}

export function Overview({
  live,
  onSelectSlot,
}: {
  live: LiveState;
  onSelectSlot: (slot: SlotRecord) => void;
}) {
  const [span, setSpan] = useState(3_600);
  const [now, setNow] = useState(() => Math.floor(Date.now() / 1000));
  useEffect(() => {
    const timer = window.setInterval(
      () => setNow(Math.floor(Date.now() / 1000)),
      15_000,
    );
    return () => window.clearInterval(timer);
  }, []);
  const range = useMemo<[number, number]>(() => [now - span, now], [now, span]);
  const step = Math.max(5, Math.round(span / 300));

  return (
    <>
      <div className="filters" role="toolbar" aria-label="Time range">
        <span className="label">Range</span>
        {RANGES.map(([label, seconds]) => (
          <button
            key={label}
            aria-pressed={span === seconds}
            onClick={() => setSpan(seconds)}
          >
            {label}
          </button>
        ))}
        <span className="hint">
          charts refresh every 15 s; slots and events are live
        </span>
      </div>
      <Kpis />
      <SlotStrip slots={live.slots} onSelect={onSelectSlot} />
      <EventLane events={live.events} range={range} />
      <div className="grid2">
        {CHARTS.map((spec) => (
          <ChartCard
            key={spec.title}
            spec={spec}
            range={range}
            step={step}
            live={live}
          />
        ))}
      </div>
      <Failures />
    </>
  );
}

function ChartCard({
  spec,
  range,
  step,
  live,
}: {
  spec: ChartSpec;
  range: [number, number];
  step: number;
  live: LiveState;
}) {
  const { data, error, loading } = usePoll(
    () => loadChart(spec, range, step),
    15_000,
    [range[0], range[1], step],
  );
  return (
    <Chart
      title={spec.title}
      sub={spec.sub}
      unit={spec.unit}
      series={data ?? []}
      range={range}
      events={live.events}
      loading={loading}
      error={error}
      grouped={spec.grouped ?? false}
    />
  );
}

const TILES: {
  label: string;
  query: string;
  format: (v: number | null) => string;
  note: string;
}[] = [
  {
    label: "Cluster non-vote TPS",
    query:
      'sum(rate(chain_block_transactions_total{kind="non_vote"}[1m])) or sum(chain_perf_tps{kind="non_vote"})',
    format: (v) => compact(v),
    note: "last minute",
  },
  {
    label: "Our TPS",
    query: "sum(rate(chain_ours_transactions_total[1m]))",
    format: (v) => compact(v),
    note: "landed, last minute",
  },
  {
    label: "Our success on chain",
    query:
      'sum(increase(chain_ours_transactions_total{status="ok"}[5m])) / sum(increase(chain_ours_transactions_total[5m]))',
    format: (v) => percent(v),
    note: "last 5 min",
  },
  {
    label: "Skip rate",
    query:
      '(sum(increase(chain_slots_total{status="skipped"}[5m])) or vector(0)) / sum(increase(chain_slots_total[5m]))',
    format: (v) => percent(v),
    note: "last 5 min",
  },
  {
    label: "Slot time p50",
    query: 'chain_slot_interval_seconds{quantile="0.5"}',
    format: (v) => seconds(v),
    note: "from slot notifications",
  },
  {
    label: "Settle time p50",
    query: 'avg(chain_tx_settle_seconds{quantile="0.5",outcome="landed"})',
    format: (v) => seconds(v),
    note: "our landed transactions",
  },
  {
    label: "Pending feature gates",
    query: 'chain_features{state="pending"}',
    format: (v) => compact(v),
    note: "activate next epoch",
  },
];

function Kpis() {
  const { data } = usePoll(
    () =>
      Promise.all(
        TILES.map((t) =>
          queryInstant(t.query).then(
            (r) => r[0]?.value ?? null,
            () => null,
          ),
        ),
      ),
    5_000,
    [],
  );
  return (
    <div className="kpis">
      {TILES.map((tile, i) => (
        <div className="tile" key={tile.label}>
          <div className="label">{tile.label}</div>
          <div className="value">{tile.format(data?.[i] ?? null)}</div>
          <div className="note">{tile.note}</div>
        </div>
      ))}
    </div>
  );
}

/** Failure kinds are many classes that all matter: a table, not more colours. */
function Failures() {
  const { data } = usePoll(
    async () => {
      const [client, chain] = await Promise.all([
        queryInstant(
          'sum by (actor, outcome, error) (increase(chain_tx_outcome_total{outcome!="landed"}[15m])) > 0',
        ),
        queryInstant(
          'sum by (actor_kind, error_program, error) (increase(chain_ours_transactions_total{status="failed"}[15m])) > 0',
        ),
      ]);
      return {
        client: client.sort((a, b) => b.value - a.value).slice(0, 15),
        chain: chain.sort((a, b) => b.value - a.value).slice(0, 15),
      };
    },
    15_000,
    [],
  );
  return (
    <div className="grid2">
      <section className="card">
        <h2>Unsuccessful sends, last 15 min</h2>
        <p className="sub">as each service classified them</p>
        <FailureTable
          rows={(data?.client ?? []).map((r) => [
            r.labels.actor,
            r.labels.outcome,
            r.labels.error,
            r.value,
          ])}
          heads={["actor", "outcome", "error"]}
        />
      </section>
      <section className="card">
        <h2>Failed on chain, last 15 min</h2>
        <p className="sub">
          from blocks: which program rejected our transaction
        </p>
        <FailureTable
          rows={(data?.chain ?? []).map((r) => [
            r.labels.actor_kind,
            r.labels.error_program,
            r.labels.error,
            r.value,
          ])}
          heads={["actor", "program", "error"]}
        />
      </section>
    </div>
  );
}

function FailureTable({
  rows,
  heads,
}: {
  rows: [string | undefined, string | undefined, string | undefined, number][];
  heads: string[];
}) {
  if (rows.length === 0) return <div className="empty">none</div>;
  return (
    <table>
      <thead>
        <tr>
          {heads.map((h) => (
            <th key={h}>{h}</th>
          ))}
          <th className="num">count</th>
        </tr>
      </thead>
      <tbody>
        {rows.map(([a, b, c, n]) => (
          <tr key={`${a}-${b}-${c}`}>
            <td>{a || "–"}</td>
            <td>{b || "–"}</td>
            <td className="mono">{c || "–"}</td>
            <td className="num">{Math.round(n)}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}
