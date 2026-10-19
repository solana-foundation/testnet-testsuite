// Compose a scenario from its parameters (crates/scenario-spec): start from
// a preset or an existing overlay, change tokens, venues, makers, takers and
// bots, and see the load it should put on chain before writing the overlay
// (config/scale-<name>.toml) and starting `up` with it.

import { useEffect, useMemo, useState } from "react";

import {
  VENUES,
  composeSpec,
  estimateSpec,
  getJson,
  startJob,
  type Chain,
  type ComposeInfo,
  type EstimateResponse,
  type Spec,
  type Status,
  type Venue,
} from "../api";
import { compact, percent, seconds } from "../format";
import { usePoll } from "../hooks";

const VENUE_LABEL: Record<Venue, string> = {
  manifest: "Manifest",
  "orca-whirlpool": "Orca",
  "raydium-clmm": "Raydium",
};

const LIMIT_LABEL = {
  host: "host execution",
  block: "block compute",
  account: "busiest pool",
} as const;

/** Whole numbers with separators: rates here span 1 to 10,000s. */
function whole(value: number) {
  return Math.round(value).toLocaleString();
}

function meterColor(utilisation: number) {
  if (utilisation >= 1) return "var(--critical)";
  if (utilisation >= 0.7) return "var(--warning)";
  return "var(--good)";
}

function toggle<T>(list: T[], item: T, on: boolean): T[] {
  return on
    ? list.includes(item)
      ? list
      : [...list, item]
    : list.filter((x) => x !== item);
}

function NumberField({
  label,
  value,
  onChange,
  min,
  max,
  step,
  width = 90,
}: {
  label: string;
  value: number;
  onChange: (value: number) => void;
  min?: number;
  max?: number;
  step?: number;
  width?: number;
}) {
  // Keep the text while typing ("0." or ""), commit numbers only.
  const [text, setText] = useState(String(value));
  useEffect(() => {
    if (Number(text) !== value) setText(String(value));
  }, [value]);
  return (
    <label>
      {label}
      <input
        type="number"
        value={text}
        min={min}
        max={max}
        step={step}
        style={{ width }}
        onChange={(e) => {
          setText(e.target.value);
          const n = Number(e.target.value);
          if (e.target.value !== "" && Number.isFinite(n)) onChange(n);
        }}
      />
    </label>
  );
}

export function Compose({ status }: { status: Status | null }) {
  const info = usePoll(() => getJson<ComposeInfo>("/api/compose"), 60_000, []);
  const [spec, setSpec] = useState<Spec | null>(null);
  const [source, setSource] = useState("x10");
  const [chainName, setChainName] = useState<string | null>(null);
  const [targetTps, setTargetTps] = useState<number | null>(null);
  const [result, setResult] = useState<EstimateResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [written, setWritten] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const sources = useMemo(() => {
    const data = info.data;
    if (!data) return [];
    const presetNames = new Set(data.presets.map((p) => p.name));
    return [
      ...data.presets.map((p) => ({ ...p, label: `preset ${p.name}` })),
      ...data.overlays
        .filter((o) => !presetNames.has(o.name))
        .map((o) => ({ ...o, label: `overlay ${o.name}` })),
    ];
  }, [info.data]);
  const chains = info.data?.chains ?? [];
  const chain: Chain | undefined =
    chains.find((c) => c.name === chainName) ?? chains[0];

  // Load the chosen source once the presets arrive, and on every change.
  useEffect(() => {
    const found = sources.find((s) => s.name === source);
    if (found) setSpec(structuredClone(found.spec));
  }, [source, sources.length]);

  // Re-estimate (debounced) whenever the spec, chain or target changes.
  useEffect(() => {
    if (!spec || !chain) return;
    const timer = setTimeout(() => {
      estimateSpec(spec, chain, targetTps)
        .then((r) => {
          setResult(r);
          setError(null);
        })
        .catch((e: unknown) => setError(String(e)));
    }, 250);
    return () => clearTimeout(timer);
  }, [spec, chain, targetTps]);

  if (!spec || !info.data || !chain) {
    return (
      <section className="card">
        <h2>Compose a scenario</h2>
        <p className="hint">{info.error ? String(info.error) : "loading…"}</p>
      </section>
    );
  }
  const catalog = info.data.catalog;
  const update = (change: (draft: Spec) => void) => {
    const draft = structuredClone(spec);
    change(draft);
    setSpec(draft);
    setWritten(null);
  };
  const running = status?.job ?? null;
  const e = result?.estimate;

  const write = async (andUp: boolean) => {
    setBusy(true);
    setError(null);
    try {
      const { name } = await composeSpec(spec);
      setWritten(name);
      if (andUp) {
        await startJob({
          command: "up",
          operands: [],
          scale: name,
          run_id: null,
          janitor: false,
          close_programs: false,
          confirm: null,
        });
      }
    } catch (err) {
      setError(String(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="card">
      <div className="card-head">
        <h2>Compose a scenario</h2>
        <span className="sub">
          parameters → overlay config/scale-&lt;name&gt;.toml → <code>up</code>
        </span>
      </div>
      <div className="form" style={{ marginTop: 8 }}>
        <label>
          start from
          <select value={source} onChange={(ev) => setSource(ev.target.value)}>
            {sources.map((s) => (
              <option key={s.name} value={s.name}>
                {s.label}
              </option>
            ))}
          </select>
        </label>
        <label>
          estimate on
          <select
            value={chain.name}
            onChange={(ev) => setChainName(ev.target.value)}
          >
            {chains.map((c) => (
              <option key={c.name} value={c.name}>
                {c.name}
              </option>
            ))}
          </select>
        </label>
      </div>

      <div className="grid2" style={{ marginTop: 6 }}>
        <div>
          <h3 className="group">Assets and pools</h3>
          <div className="form">
            {catalog.map((t) => (
              <label key={t} className="check">
                <input
                  type="checkbox"
                  checked={spec.tokens.includes(t)}
                  onChange={(ev) =>
                    update((d) => {
                      d.tokens = toggle(d.tokens, t, ev.target.checked);
                      if (!ev.target.checked) {
                        d.crosses = d.crosses.filter(
                          ([a, b]) => a !== t && b !== t,
                        );
                        d.makers.thin_tokens = d.makers.thin_tokens.filter(
                          (x) => x !== t,
                        );
                      }
                    })
                  }
                />
                {t}
              </label>
            ))}
          </div>
          <div className="form" style={{ marginTop: 6 }}>
            <span className="hint">each token / USDC on</span>
            {VENUES.map((v) => (
              <label key={v} className="check">
                <input
                  type="checkbox"
                  checked={spec.venues.includes(v)}
                  onChange={(ev) =>
                    update((d) => {
                      d.venues = toggle(d.venues, v, ev.target.checked);
                    })
                  }
                />
                {VENUE_LABEL[v]}
              </label>
            ))}
          </div>
          <Crosses spec={spec} update={update} />

          <h3 className="group">Makers per pool</h3>
          <div className="form">
            <NumberField
              label="Manifest ladders"
              value={spec.makers.ladders}
              min={0}
              max={3}
              onChange={(n) => update((d) => void (d.makers.ladders = n))}
            />
            <NumberField
              label="AMM LP ranges"
              value={spec.makers.ranges}
              min={0}
              max={3}
              onChange={(n) => update((d) => void (d.makers.ranges = n))}
            />
            <NumberField
              label="AMM pool-arbs"
              value={spec.makers.pool_arbs}
              min={0}
              max={3}
              onChange={(n) => update((d) => void (d.makers.pool_arbs = n))}
            />
            <label className="check">
              <input
                type="checkbox"
                checked={spec.makers.capital == null}
                onChange={(ev) =>
                  update((d) => {
                    d.makers.capital = ev.target.checked
                      ? null
                      : (e?.capital ?? "10");
                  })
                }
              />
              capital sized to the flow
              {spec.makers.capital == null && e ? ` (×${e.capital})` : ""}
            </label>
            {spec.makers.capital != null && (
              <NumberField
                label="capital ×"
                value={Number(spec.makers.capital)}
                min={0.1}
                step={1}
                onChange={(n) =>
                  update((d) => void (d.makers.capital = String(n)))
                }
              />
            )}
          </div>
          {spec.tokens.length > 0 && (
            <div className="form" style={{ marginTop: 6 }}>
              <span className="hint">one maker of each kind on</span>
              {spec.tokens.map((t) => (
                <label key={t} className="check">
                  <input
                    type="checkbox"
                    checked={spec.makers.thin_tokens.includes(t)}
                    onChange={(ev) =>
                      update((d) => {
                        d.makers.thin_tokens = toggle(
                          d.makers.thin_tokens,
                          t,
                          ev.target.checked,
                        );
                      })
                    }
                  />
                  {t}
                </label>
              ))}
            </div>
          )}

          <h3 className="group">Takers and arb bots</h3>
          <div className="form">
            <NumberField
              label="takers"
              value={spec.takers.count}
              min={1}
              width={100}
              onChange={(n) =>
                update((d) => void (d.takers.count = Math.round(n)))
              }
            />
            <NumberField
              label="trades / taker / min"
              value={spec.takers.trades_per_min}
              min={0.1}
              step={0.5}
              onChange={(n) =>
                update((d) => void (d.takers.trades_per_min = n))
              }
            />
            <NumberField
              label="median trade $"
              value={spec.takers.median_usd}
              min={1}
              onChange={(n) => update((d) => void (d.takers.median_usd = n))}
            />
            <NumberField
              label="large-trade chance"
              value={spec.takers.large_probability}
              min={0}
              max={1}
              step={0.01}
              onChange={(n) =>
                update((d) => void (d.takers.large_probability = n))
              }
            />
            <NumberField
              label="exact-out chance"
              value={spec.takers.exact_out_probability}
              min={0}
              max={1}
              step={0.05}
              onChange={(n) =>
                update((d) => void (d.takers.exact_out_probability = n))
              }
            />
            <NumberField
              label="slippage bps"
              value={spec.takers.slippage_bps}
              min={0}
              max={10000}
              onChange={(n) =>
                update((d) => void (d.takers.slippage_bps = Math.round(n)))
              }
            />
            <NumberField
              label="arb bots"
              value={spec.arb.bots}
              min={0}
              onChange={(n) => update((d) => void (d.arb.bots = Math.round(n)))}
            />
          </div>
          <div className="form" style={{ marginTop: 6 }}>
            <label>
              target tx/s
              <input
                type="number"
                min={1}
                value={targetTps ?? ""}
                placeholder="optional"
                style={{ width: 100 }}
                onChange={(ev) =>
                  setTargetTps(
                    ev.target.value === "" ? null : Number(ev.target.value),
                  )
                }
              />
            </label>
            <button
              disabled={result?.target_takers == null}
              onClick={() =>
                result?.target_takers != null &&
                update((d) => void (d.takers.count = result.target_takers!))
              }
            >
              Set takers
              {result?.target_takers != null
                ? ` to ${result.target_takers.toLocaleString()}`
                : ""}
            </button>
          </div>
        </div>

        <div>
          <h3 className="group">Estimate</h3>
          {e ? <EstimateView result={result!} /> : <p className="hint">…</p>}
          {result?.error && <div className="error">{result.error}</div>}
          <div className="form" style={{ marginTop: 12 }}>
            <button
              disabled={busy || !!result?.error}
              onClick={() => write(false)}
            >
              Write overlay
            </button>
            <button
              disabled={busy || !!result?.error || running != null}
              onClick={() => write(true)}
            >
              Write and run up
            </button>
            {result && (
              <span className="hint">
                → <code>config/scale-{result.name}.toml</code>
                {written === result.name && " written"}
              </span>
            )}
          </div>
          <p className="hint">
            An overlay is never rewritten: the same spec reuses its file, so a
            running scenario&apos;s config does not change under it. Prices come
            from Binance when it is written.
          </p>
          {error && <div className="error">{error}</div>}
        </div>
      </div>
    </section>
  );
}

function Crosses({
  spec,
  update,
}: {
  spec: Spec;
  update: (change: (draft: Spec) => void) => void;
}) {
  const [a, setA] = useState("");
  const [b, setB] = useState("");
  const options = spec.tokens;
  return (
    <div className="form" style={{ marginTop: 6 }}>
      <span className="hint">Orca cross pools</span>
      {spec.crosses.map(([x, y]) => (
        <button
          key={`${x}/${y}`}
          title="remove"
          onClick={() =>
            update((d) => {
              d.crosses = d.crosses.filter(([p, q]) => !(p === x && q === y));
            })
          }
        >
          {x}/{y} ×
        </button>
      ))}
      <select value={a} onChange={(ev) => setA(ev.target.value)}>
        <option value="">base</option>
        {options.map((t) => (
          <option key={t}>{t}</option>
        ))}
      </select>
      <select value={b} onChange={(ev) => setB(ev.target.value)}>
        <option value="">quote</option>
        {options
          .filter((t) => t !== a)
          .map((t) => (
            <option key={t}>{t}</option>
          ))}
      </select>
      <button
        disabled={!a || !b || a === b}
        onClick={() => {
          update((d) => void d.crosses.push([a, b]));
          setA("");
          setB("");
        }}
      >
        Add
      </button>
    </div>
  );
}

function EstimateView({ result }: { result: EstimateResponse }) {
  const e = result.estimate;
  return (
    <>
      <div className="kpis" style={{ marginTop: 0 }}>
        <div className="tile">
          <div className="label">expected tx/s</div>
          <div className="value">{whole(e.tps_capped)}</div>
          <div className="note">
            {e.tps_capped < e.tps
              ? `${whole(e.tps)} offered, capped by ${LIMIT_LABEL[e.bottleneck ?? "host"]}`
              : `${whole(e.taker_tx_per_s)} takers + ${e.maker_tx_per_s.toFixed(1)} makers + ${e.arb_tx_per_s.toFixed(2)} arb`}
          </div>
        </div>
        <div className="tile">
          <div className="label">participants</div>
          <div className="value">{whole(e.participants)}</div>
          <div className="note">
            {e.takers.toLocaleString()} takers · {e.makers} makers · {e.bots}{" "}
            bots · {e.pools} pools
          </div>
        </div>
        <div className="tile">
          <div className="label">CU per block</div>
          <div className="value">{compact(e.cu_per_block)}</div>
          <div className="note">
            {compact(e.cu_per_s)} CU/s at {e.chain.slot_ms} ms slots
          </div>
        </div>
      </div>
      <p className="hint" style={{ marginTop: 8 }}>
        takers offer {whole(e.offered_swaps_per_s)} swaps/s on schedule; each
        waits {seconds(e.trade_seconds)} for its trade to settle before the
        next, which takes it to {whole(e.taker_tx_per_s)}
      </p>
      <table style={{ marginTop: 8 }}>
        <tbody>
          {e.headroom.map((h) => (
            <tr key={h.limit}>
              <td>{LIMIT_LABEL[h.limit]}</td>
              <td>
                <span className="meter" style={{ width: 120 }}>
                  <span
                    style={{
                      width: `${Math.min(100, h.utilisation * 100)}%`,
                      background: meterColor(h.utilisation),
                    }}
                  />
                </span>
              </td>
              <td className="num">{percent(h.utilisation, 0)}</td>
              <td className="hint">{h.detail}</td>
            </tr>
          ))}
        </tbody>
      </table>
      <table style={{ marginTop: 8 }}>
        <thead>
          <tr>
            <th>venue</th>
            <th className="num">legs/s</th>
            <th className="num">share</th>
          </tr>
        </thead>
        <tbody>
          {e.venues.map((v) => (
            <tr key={v.venue}>
              <td>{VENUE_LABEL[v.venue]}</td>
              <td className="num">{whole(v.legs_per_s)}</td>
              <td className="num">{percent(v.share, 0)}</td>
            </tr>
          ))}
        </tbody>
      </table>
      <p className="hint">
        Venue shares follow the router&apos;s best price (calibrated from the
        x1500 runs); busiest pool: {e.hottest[0]?.pool ?? "–"} at{" "}
        {whole(e.hottest[0]?.legs_per_s ?? 0)} swaps/s.
      </p>
      <p className="hint">
        bootstrap ~{whole(e.bootstrap.transactions)} transactions, ~
        {seconds(e.bootstrap.seconds)} · SOL {e.sol.fees_per_hour.toFixed(1)}/h
        fees, {e.sol.rent.toFixed(1)} rent, {whole(e.sol.float)} wallet float
      </p>
      {e.warnings.map((w) => (
        <p key={w} className="warn">
          <span aria-hidden>⚠</span> {w}
        </p>
      ))}
    </>
  );
}
