"""Generate the scenario Grafana dashboard (ops/grafana/dashboards/scenario.json).

    python3 bootstrap/scenario/dashboard.py ops/grafana/dashboards/scenario.json

Edit panels here rather than in the JSON, so layout and IDs stay consistent.
Histograms are Prometheus summaries (metrics-exporter-prometheus default),
hence the `quantile` label selectors.
"""
import json, sys
panels = []
y = 0
def row(title):
    global y
    panels.append({"type": "row", "title": title, "collapsed": False, "gridPos": {"h": 1, "w": 24, "x": 0, "y": y}, "panels": []})
    y += 1
def ts(title, exprs, unit="short", x=0, w=12, h=8, desc=""):
    panels.append({
        "type": "timeseries", "title": title, "description": desc,
        "datasource": {"type": "prometheus", "uid": "prometheus"},
        "gridPos": {"h": h, "w": w, "x": x, "y": y},
        "fieldConfig": {"defaults": {"unit": unit}, "overrides": []},
        "options": {"legend": {"displayMode": "table", "placement": "right", "calcs": ["lastNotNull"]}, "tooltip": {"mode": "multi"}},
        "targets": [{"refId": chr(65 + i), "expr": e, "legendFormat": l} for i, (e, l) in enumerate(exprs)],
    })
def stat(title, expr, unit="short", x=0, w=6, h=4):
    panels.append({
        "type": "stat", "title": title, "datasource": {"type": "prometheus", "uid": "prometheus"},
        "gridPos": {"h": h, "w": w, "x": x, "y": y}, "fieldConfig": {"defaults": {"unit": unit}, "overrides": []},
        "options": {"reduceOptions": {"calcs": ["lastNotNull"]}},
        "targets": [{"refId": "A", "expr": expr}],
    })
def nl(h):
    global y
    y += h

row("Scenario health")
stat("Routed swaps landed (5m)", 'sum(increase(flow_swaps_total{status="landed"}[5m])) or vector(0)', x=0)
stat("Routed swaps failed (5m)", 'sum(increase(flow_swaps_total{status="failed"}[5m])) or vector(0)', x=6)
stat("Maker errors (5m)", '(sum(increase(mm_errors_total[5m])) or vector(0)) + (sum(increase(mm_panics_total[5m])) or vector(0))', x=12)
stat("Flow notional (USD/min)", 'sum(rate(flow_notional_micro_usd_total[5m])) * 60 / 1e6', unit="currencyUSD", x=18)
nl(4)
row("Oracle")
ts("Oracle pair prices", [("mm_oracle_price", "{{pair}}")], x=0)
ts("Oracle updates / s", [("sum by (symbol) (rate(oracle_price_updates_total[1m]))", "{{symbol}}")], x=12)
nl(8)
row("Venues vs oracle")
ts("Venue price deviation from oracle", [("mm_deviation_bps", "{{maker}} ({{venue}})")], unit="none", x=0,
   desc="(venue price - oracle) / oracle in bps; arbitrageurs pull AMMs back inside their threshold")
ts("Venue prices", [("mm_venue_price", "{{maker}}")], x=12)
nl(8)
ts("Maker transactions by venue / min", [
    ("sum by (venue, action) (rate(mm_actions_total[5m])) * 60", "{{venue}} {{action}}"),
    ("sum by (venue) (rate(flow_route_legs_total[5m])) * 60", "{{venue}} routed legs")], x=0)
ts("Simulated CU per venue tx (p95)", [
    ('orca_whirlpool_simulated_compute_units{quantile="0.95"}', "orca"),
    ('raydium_clmm_simulated_compute_units{quantile="0.95"}', "raydium"),
    ('manifest_simulated_compute_units{quantile="0.95"}', "manifest")], x=12)
nl(8)
row("Market makers")
ts("Inventory value (USD)", [("mm_inventory_value_usd", "{{maker}}")], unit="currencyUSD", x=0)
ts("Maker actions / min", [("sum by (maker, action) (rate(mm_actions_total[5m])) * 60", "{{maker}} {{action}}")], x=12)
nl(8)
ts("Inventory by token", [("mm_inventory", "{{maker}} {{token}}")], x=0)
ts("Quote skew (bps) and arb volume (USD/min)", [
    ("mm_skew_bps", "skew {{maker}}"),
    ("sum by (maker) (rate(mm_arb_volume_micro_usd_total[5m])) * 60 / 1e6", "arb {{maker}}")], x=12)
nl(8)
ts("Maker errors / min", [("sum by (maker, stage) (rate(mm_errors_total[5m])) * 60", "{{maker}} {{stage}}")], x=0)
ts("Maker tick latency (p95)", [('mm_tick_seconds{quantile="0.95"}', "{{maker}}")], unit="s", x=12)
nl(8)
row("Uninformed flow + router")
ts("Routed swaps / min", [("sum by (pair, status, size) (rate(flow_swaps_total[5m])) * 60", "{{pair}} {{status}} {{size}}")], x=0)
ts("Swap outcomes / min", [("sum by (outcome) (rate(flow_swaps_total[5m])) * 60", "{{outcome}}")], x=12,
   desc="landed, or why not: unaffordable / no_price / quote_failed / preflight_rejected / send_failed / landed_failed / expired / rpc_ambiguous / error")
nl(8)
ts("Cost vs oracle (bps)", [
    ('flow_cost_vs_oracle_bps{quantile="0.5"}', "p50 {{pair}}"),
    ('flow_cost_vs_oracle_bps{quantile="0.95"}', "p95 {{pair}}")], unit="none", x=0,
   desc="value given up versus the oracle: spread + fees + impact paid by uninformed flow")
ts("Landed vs quoted slippage (bps)", [('flow_slippage_bps{quantile="0.95"}', "p95 {{pair}}"), ('flow_slippage_bps{quantile="0.5"}', "p50 {{pair}}")], unit="none", x=12)
nl(8)
ts("Route legs by venue / min", [("sum by (venue) (rate(flow_route_legs_total[5m])) * 60", "{{venue}}")], x=0)
ts("Router quote latency", [('router_quote_seconds{quantile="0.5"}', "p50 {{execution}}"), ('router_quote_seconds{quantile="0.95"}', "p95 {{execution}}")], unit="s", x=12)
nl(8)
ts("Router quotes and errors / min", [
    ("sum by (execution) (rate(router_quotes_total[5m])) * 60", "quotes {{execution}}"),
    ("sum by (status) (rate(router_quote_errors_total[5m])) * 60", "errors {{status}}")], x=0)
nl(8)

dash = {"uid": "mtm-scenario", "title": "mtm — market-making scenario", "tags": ["mtm"], "timezone": "browser",
        "schemaVersion": 39, "version": 1, "refresh": "10s", "time": {"from": "now-30m", "to": "now"}, "panels": panels}
for i, p in enumerate(panels):
    p["id"] = i + 1
json.dump(dash, open(sys.argv[1], "w"), indent=2)
print(len(panels), "panels")
