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
def table(title, exprs, x=0, w=24, h=8, desc="", sort=None):
    """Instant-query table; each expr's labels become columns."""
    panels.append({
        "type": "table", "title": title, "description": desc,
        "datasource": {"type": "prometheus", "uid": "prometheus"},
        "gridPos": {"h": h, "w": w, "x": x, "y": y},
        "options": {"showHeader": True, "sortBy": sort or []},
        "transformations": [{"id": "merge"}, {"id": "organize", "options": {"excludeByName": {"Time": True, "__name__": True, "job": True, "instance": True}}}],
        "targets": [{"refId": chr(65 + i), "expr": e, "legendFormat": l, "format": "table", "instant": True} for i, (e, l) in enumerate(exprs)],
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
stat("Arb trades landed (5m)", 'sum(increase(arb_trades_total{status="landed"}[5m])) or vector(0)', x=0)
stat("Arb net profit (USD, range)", '(sum(increase(arb_profit_micro_usd_total[$__range])) - sum(increase(arb_loss_micro_usd_total[$__range]))) / 1e6 or vector(0)', unit="currencyUSD", x=6)
nl(4)
# --- chain-watch (services/chain-watch): the cluster under the scenario --------
row("Cluster (chain-watch)")
stat("Non-vote TPS (blocks, 1m)", 'sum(rate(chain_block_transactions_total{kind="non_vote"}[1m]))', x=0)
stat("Our TPS (on chain, 1m)", 'sum(rate(chain_ours_transactions_total[1m])) or vector(0)', x=6)
stat("Our success on chain (5m)", 'sum(increase(chain_ours_transactions_total{status="ok"}[5m])) / sum(increase(chain_ours_transactions_total[5m]))', unit="percentunit", x=12)
stat("Skip rate (5m)", '(sum(increase(chain_slots_total{status="skipped"}[5m])) or vector(0)) / sum(increase(chain_slots_total[5m]))', unit="percentunit", x=18)
nl(4)
stat("Slot time p50 (ws)", 'chain_slot_interval_seconds{quantile="0.5"}', unit="s", x=0)
stat("Epoch progress", 'chain_epoch_progress', unit="percentunit", x=6)
stat("Pending feature gates", 'chain_features{state="pending"}', x=12)
stat("Block pipeline lag (slots)", 'chain_block_lag_slots', x=18)
nl(4)
ts("Transactions / s", [
    ('sum by (kind) (rate(chain_block_transactions_total[1m]))', "{{kind}} (blocks)"),
    ('sum(rate(chain_ours_transactions_total[1m]))', "ours (blocks)"),
    ('sum(rate(chain_block_failed_transactions_total[1m]))', "failed (blocks)"),
    ('chain_perf_tps', "{{kind}} (perf samples, 60 s)")], x=0,
   desc="counted from every confirmed block; perf samples are the node's own 60 s windows (cross-check, and the only source in accounts mode)")
ts("Slots / s by leader client", [
    ('sum by (leader_client, status) (rate(chain_slots_total[1m]))', "{{leader_client}} {{status}}")], x=12,
   desc="produced vs skipped per producing client (clientId from getClusterNodes)")
nl(8)
ts("Slot interval (ws) and pacing", [
    ('chain_slot_interval_seconds{quantile="0.5"}', "p50"),
    ('chain_slot_interval_seconds{quantile="0.99"}', "p99"),
    ('chain_perf_slot_seconds', "perf sample avg")], unit="s", x=0)
ts("Commitment gaps (slots)", [
    ('chain_slot{commitment="processed"} - ignoring(commitment) chain_slot{commitment="confirmed"}', "processed - confirmed"),
    ('chain_slot{commitment="confirmed"} - ignoring(commitment) chain_slot{commitment="finalized"}', "confirmed - finalized")], x=12,
   desc="consensus lag: how far confirmation and finality trail the tip")
nl(8)
ts("Block compute (per block)", [
    ('chain_block_cu{quantile="0.5"}', "CU p50"), ('chain_block_cu{quantile="0.99"}', "CU p99"),
    ('chain_block_cost_units{quantile="0.5"}', "cost p50"), ('chain_block_cost_units{quantile="0.99"}', "cost p99")], x=0,
   desc="cost units are what the block limit counts; set chain_watch.block_cost_limit for the fill ratio")
ts("Priority price (micro-lamports / CU)", [
    ('chain_prio_price_micro_lamports{quantile="0.5"}', "p50 {{scope}}"),
    ('chain_prio_price_micro_lamports{quantile="0.9"}', "p90 {{scope}}")], x=12)
nl(8)
ts("Stake by client", [('chain_stake_ratio', "{{client}}"), ('chain_delinquent_stake_ratio', "delinquent")], unit="percentunit", x=0)
ts("Stake by version", [('chain_stake_by_version_ratio > 0', "{{version}}")], unit="percentunit", x=12,
   desc="a majority change is a timeline event (majority_version) in chain-watch's run store")
nl(8)
table("Feature gates (on chain)", [
    ('chain_feature_activation_slot', "activated"),
    ('chain_feature_active == 0', "pending")], h=10,
   desc="every Feature-program account: Value #A = activation slot; pending ones (Value #B = 0) activate at the next epoch boundary. Full history with times: chain-watch GET /features",
   sort=[{"displayName": "Value #A", "desc": True}])
nl(10)
ts("RPC latency p95 (chain-watch)", [('chain_rpc_seconds{quantile="0.95"}', "{{method}}")], unit="s", x=0)
ts("RPC errors / min (chain-watch)", [('sum by (method, kind) (rate(chain_rpc_errors_total[5m])) * 60', "{{method}} {{kind}}")], x=12)
nl(8)

# --- every service's transactions, labelled at the source (mtm_chain::tag) ----
row("Our transactions")
ts("Outcomes / s (client view)", [('sum by (outcome) (rate(chain_tx_outcome_total[1m]))', "{{outcome}}")], x=0,
   desc="landed / failed (on chain) / simulation_failed / rejected / expired / ambiguous / client, from ChainClient in every service")
ts("Landing rate by actor (5m)", [
    ('sum by (actor) (rate(chain_tx_outcome_total{outcome="landed"}[5m])) / sum by (actor) (rate(chain_tx_outcome_total[5m]))', "{{actor}}")], unit="percentunit", x=12)
nl(8)
ts("Failure kinds / min", [
    ('sum by (outcome, error) (rate(chain_tx_outcome_total{outcome!="landed"}[5m])) * 60', "{{outcome}} {{error}}"),
    ('sum by (error, error_program) (rate(chain_ours_transactions_total{status="failed"}[5m])) * 60', "on chain {{error_program}} {{error}}")], x=0)
ts("Settle latency (send → commitment)", [
    ('chain_tx_settle_seconds{quantile="0.5",outcome="landed"}', "p50 {{action}}"),
    ('chain_tx_settle_seconds{quantile="0.95",outcome="landed"}', "p95 {{action}}")], unit="s", x=12)
nl(8)
ts("CU per transaction by program", [
    ('chain_ours_cu{quantile="0.5"}', "p50 {{program}}"), ('chain_ours_cu{quantile="0.95"}', "p95 {{program}}")], x=0,
   desc="consumed on chain; a step change after a feature activation is a repricing")
ts("CU limit used", [('chain_ours_cu_limit_used_ratio{quantile="0.5"}', "p50 {{program}}"), ('chain_ours_cu_limit_used_ratio{quantile="0.95"}', "p95 {{program}}")], unit="percentunit", x=12,
   desc="consumed / requested; low values waste block space and priority fees")
nl(8)
ts("Our transactions by leader client", [
    ('sum by (leader_client, status) (rate(chain_ours_by_leader_client_total[5m]))', "{{leader_client}} {{status}}")], x=0)
ts("Fees (SOL / min) by actor kind", [
    ('sum by (actor_kind) (rate(chain_ours_fees_lamports_total[5m])) * 60 / 1e9', "{{actor_kind}}")], x=12)
nl(8)

row("Host")
ts("CPU busy", [('1 - avg(rate(node_cpu_seconds_total{mode="idle"}[1m]))', "all cores")], unit="percentunit", x=0)
ts("Memory available and TCP sockets", [
    ('node_memory_MemAvailable_bytes / node_memory_MemTotal_bytes', "memory available"),
    ('node_sockstat_TCP_inuse / 30000', "TCP in use / 30k"),
    ('node_sockstat_TCP_tw / 30000', "TIME_WAIT / 30k")], unit="percentunit", x=12,
   desc="ephemeral-port exhaustion (EADDRNOTAVAIL) ended the x2000 run; sockets scaled by ~30k ports")
nl(8)

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
ts("Hedged at the oracle (USD / 5m)", [
    ("sum by (maker, side) (increase(mm_hedge_micro_usd_total[5m])) / 1e6", "{{maker}} {{side}}")], unit="currencyUSD", x=0,
   desc="inventory laid off at the oracle price (CEX-hedge model): sell_base = maker was long base")
ts("Hedge cost (USD / 5m)", [
    ("sum by (maker) (increase(mm_hedge_cost_micro_usd_total[5m])) / 1e6", "{{maker}}")], unit="currencyUSD", x=12)
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

row("Cross-venue arbitrage")
ts("Arb trades / min", [
    ("sum by (bot, status, legs) (rate(arb_trades_total[5m])) * 60", "{{bot}} {{status}} {{legs}}-leg"),
    ("sum by (bot, legs) (rate(arb_opportunities_total[5m])) * 60", "opportunities {{bot}} {{legs}}-leg")], x=0,
   desc="cycles found (above the profit floor) vs executed; below_floor = the cycle closed before sending")
ts("Arb realized PnL (USD / 5m)", [
    ("(sum by (bot) (increase(arb_profit_micro_usd_total[5m])) - sum by (bot) (increase(arb_loss_micro_usd_total[5m]))) / 1e6", "net {{bot}}"),
    ("sum by (bot) (increase(arb_loss_micro_usd_total[5m])) / -1e6", "losses {{bot}}"),
    ("arb_last_profit_usd", "last trade {{bot}}")], unit="currencyUSD", x=12,
   desc="every token the cycle touched, valued at oracle prices; SOL fees are separate (lamports panel)")
nl(8)
ts("Arb scans / min by trigger", [
    ("sum by (trigger) (rate(arb_scans_total[5m])) * 60", "{{trigger}}"),
    ("sum(rate(arb_pool_updates_total[5m])) * 60", "pool updates (ws)"),
    ("sum(rate(arb_errors_total[5m])) * 60", "errors")], x=0)
ts("Arb scan latency", [('arb_scan_seconds{quantile="0.5"}', "p50"), ('arb_scan_seconds{quantile="0.95"}', "p95")], unit="s", x=12)
nl(8)
ts("Arb fees (lamports / 5m) and cycles in flight", [
    ("sum by (bot) (increase(arb_fees_lamports_total[5m]))", "fees {{bot}}"),
    ("arb_in_flight", "in flight")], x=0,
   desc="fees include reverted cycles (lost races still pay) and first-use ATA rent")
nl(8)

def annotation(name, expr, title, color, tags=""):
    return {"name": name, "enable": True, "iconColor": color, "expr": expr, "step": "30s",
            "datasource": {"type": "prometheus", "uid": "prometheus"},
            "titleFormat": title, "tagKeys": tags, "useValueForTime": False}

# Timeline markers from chain-watch: epoch boundaries and feature-gate changes
# seen while Prometheus was scraping (older activations: the table above).
annotations = {"list": [
    annotation("Epoch boundary", "changes(chain_epoch[1m]) > 0", "Epoch boundary", "blue"),
    annotation("Feature activated", "changes(chain_feature_active[2m]) > 0", "Feature activated", "red", "feature,description"),
    annotation("Feature pending", 'changes(chain_features{state="pending"}[2m]) > 0', "Pending feature gates changed", "orange"),
]}

dash = {"uid": "mtm-scenario", "title": "mtm — market-making scenario", "tags": ["mtm"], "timezone": "browser",
        "schemaVersion": 39, "version": 1, "refresh": "10s", "time": {"from": "now-30m", "to": "now"},
        "annotations": annotations, "panels": panels}
for i, p in enumerate(panels):
    p["id"] = i + 1
json.dump(dash, open(sys.argv[1], "w"), indent=2)
print(len(panels), "panels")
