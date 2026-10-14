"""Compare `scenario.sh measure` runs side by side (markdown).

    python3 -I bootstrap/scenario/summarize.py .scenario/runs x1 x10 x100 assets
"""
import json, sys
from pathlib import Path


def load(runs, name):
    report = json.loads((Path(runs) / f"{name}-report.json").read_text())
    load = json.loads((Path(runs) / f"{name}-load.json").read_text())["summary"]
    return report, load


def fmt(stat, key="mean"):
    return "—" if not stat else f"{stat[key]:g}"


def main():
    runs, names = sys.argv[1], sys.argv[2:]
    data = {name: load(runs, name) for name in names}
    print("| | " + " | ".join(names) + " |")
    print("|---|" + "---|" * len(names))

    def row(label, f):
        print(f"| {label} | " + " | ".join(f(*data[n]) for n in names) + " |")

    row("participants", lambda r, l: str(len(r["participants"])))
    row("window (s)", lambda r, l: f"{r['windowSeconds']:g}")
    row("tx landed (unique)", lambda r, l: str(r["network"]["transactionsLanded"]))
    row("tx failed on chain", lambda r, l: str(r["network"]["transactionsFailed"]))
    row("landed TPS mean (blocks)", lambda r, l: f"{r['network']['landedTps']:g}")
    row("landed TPS peak 10 s (blocks)", lambda r, l: f"{r['network'].get('peakLandedTps', '—')}")
    row("validator TPS mean / p95 / max", lambda r, l: f"{fmt(l['tps'])} / {fmt(l['tps'], 'p95')} / {fmt(l['tps'], 'max')}"
        + ("" if l.get("tpsScope", "local validator") == "local validator" else " (network-wide)"))
    flow = lambda r: r.get("flowTrades") or {}
    row("flow swaps landed", lambda r, l: f"{sum(v for k, v in flow(r).items() if k.startswith('landed')):g}")
    row("flow swaps failed", lambda r, l: f"{sum(v for k, v in flow(r).items() if k.startswith('failed')):g}")

    def trades(r, prefix, keys):
        return sum(v for p in r["participants"] if p["role"].startswith(prefix) and isinstance(p.get("trades"), dict)
                   for k, v in p["trades"].items() if k in keys)
    row("maker actions", lambda r, l: f"{trades(r, 'maker/', {'requote', 'open', 'close', 'arb_buy', 'arb_sell'}):g}")
    row("arb cycles landed / reverted", lambda r, l: f"{trades(r, 'arb/', {'landed'}):g} / {trades(r, 'arb/', {'reverted'}):g}")
    kinds = sorted({k for r, _ in data.values() for k in r["byKind"]})
    for kind in kinds:
        def pnl(r, l, kind=kind):
            k = r["byKind"].get(kind)
            if not k:
                return "—"
            return f"{k['tradingPnlUsd']} (n={k['participants']}, median {k['medianTradingPnlUsd']}, {k['profitable']} up)"
        row(f"trading PnL: {kind}", pnl)
    row("trading PnL: total", lambda r, l: r["totals"]["tradingPnlUsd"])
    row("machine CPU % mean / max", lambda r, l: f"{fmt(l['machineCpuPct'])} / {fmt(l['machineCpuPct'], 'max')}"
        + (f" ({l['cpus']} cores)" if l.get("cpus") else ""))
    row("RPC latency ms mean / p95 / max", lambda r, l: f"{fmt(l['rpcLatencyMs'])} / {fmt(l['rpcLatencyMs'], 'p95')} / {fmt(l['rpcLatencyMs'], 'max')}")
    row("slots / s", lambda r, l: fmt(l["slotsPerSec"]))
    services = sorted({s for _, l in data.values() for s in l["services"]})
    for service in services:
        def usage(r, l, service=service):
            s = l["services"].get(service)
            if not s or not s["cpuPct"]:
                return "—"
            return f"{fmt(s['cpuPct'])}% / {fmt(s['cpuPct'], 'max')}% CPU, {fmt(s['rssMiB'], 'max')} MiB"
        row(f"{service} (mean / max CPU, max RSS)", usage)


if __name__ == "__main__":
    main()
