"""Generate scale overlays for the scenario (layered over scenario-local).

    python3 bootstrap/scenario/scale.py            # writes every overlay below
    python3 bootstrap/scenario/scale.py scale-x10  # only the named overlays
    MTM_PROFILE=scenario-local,scale-x10 ...       # select one (scenario.sh: SCENARIO_SCALE=x10)

| overlay       | takers | arb bots | makers per pool* | assets |
| ------------- | ------ | -------- | ---------------- | ------ |
| scale-x10     | 140    | 6        | 2                | 3      |
| scale-x100    | 1650   | 14       | 3                | 3      |
| scale-assets  | 1650   | 14       | 3 (1 on new)     | 11     |
| scale-x200    | 3300   | 20       | 3                | 3      |
| scale-x500    | 8250   | 30       | 3                | 3      |
| scale-x750    | 12375  | 35       | 3                | 3      |
| scale-x1000   | 16500  | 40       | 3                | 3      |
| scale-x1500   | 24750  | 45       | 3                | 3      |
| scale-x2000   | 33000  | 50       | 3                | 3      |

* per Manifest book: that many ladders; per AMM pool: that many LP ranges
  and that many oracle pool-arbs, each with different parameters. Makers
  grow slowly on purpose; participants grow by an order of magnitude.
  Maker capital grows instead (×3 at x10, ×10 at x100, ×20 at x200, ×50 at x500, ×75 at x750, ×100 at x1000, ×150 at x1500, ×200 at x2000) so the
  books and pools can absorb the larger flow. x200 is past Surfpool's
  ceiling; it is meant for solana-test-validator.

Takers keep the base per-taker arrival rate (5/min), so offered load grows
with the participant count. Maker and bot inventories are set in USD at the
price when generated; taker inventory keeps the base profile's fixed JUP /
RAY / USDC amounts, plus $5k of each new asset. Prices come from Binance, or
from SCALE_PRICES (a JSON object, e.g. '{"JUP": "0.32", "RAY": "1.84"}')
where Binance is unreachable or geo-blocked; any token it leaves out is
still fetched. The asset overlay also writes the token bootstrap config
(bootstrap/scenario/tokens-assets.json) and expects
KEYPAIR_TOKEN_<NAME>_MINT / PUBKEY_TOKEN_<NAME>_MINT for every new token.
Amounts are Decimal throughout (the TOML holds them as strings).
"""
import json, os, sys, urllib.request
from decimal import Decimal
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
PER_TAKER_RATE = 5.0  # trades / min, as in the base profile (20/min over 4 takers)

# name -> (mainnet mint for metadata, Binance symbol); USDC is pegged.
BASE_TOKENS = ["USDC", "JUP", "RAY"]
NEW_TOKENS = {
    "BONK": "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263",
    "WIF": "EKpQGSJtjMFqKZ9KQanSqYXRcF8fBopzLHYxdM65zcjm",
    "PYTH": "HZ1JovNiVvGrGNiiYvEozEVgZ58xaU3RKwX8eACQBCt3",
    "JTO": "jtojtomepa8beP8AuQc6eXt5FriJwfFMwQx2v2f9mCL",
    "ORCA": "orcaEKTdK7LKz57vaAYr9QeNsVEPfiu6QeMU1kektZE",
    "RENDER": "rndrizKT3MK1iimdxRdWabcF7Zg7AR5T4nud4EkHBof",
    "W": "85VBFQZC9TZkfaptBWjvUw7YbZjy52A6mjtPGjstQAmQ",
    "TNSR": "TNSRxcUxoT9xBG3de7PiJyTDYu7kskLqcpddxnEJAS6",
}
MAINNET = {
    "USDC": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
    "JUP": "JUPyiwrYJFskUPiHa7hkeR8VUtAeFoSYbKedZNsDvCN",
    "RAY": "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R",
    **NEW_TOKENS,
}


OVERRIDES = {name: Decimal(str(px)) for name, px in json.loads(os.environ.get("SCALE_PRICES", "{}")).items()}


def price(name):
    if name in OVERRIDES:
        return OVERRIDES[name]
    if name == "USDC":
        return Decimal(1)
    url = f"https://api.binance.com/api/v3/ticker/price?symbol={name}USDT"
    try:
        with urllib.request.urlopen(url, timeout=10) as response:
            return Decimal(json.load(response)["price"])
    except Exception as error:
        raise SystemExit(f"no price for {name} from Binance ({error}); set SCALE_PRICES") from error


def amount(usd, px):
    """USD → token amount, 3 significant figures, as a plain decimal string."""
    value = Decimal(usd) / px
    quantum = Decimal(10) ** (value.adjusted() - 2)
    return format(((value / quantum).quantize(Decimal(1)) * quantum).normalize(), "f")


def toml_value(value):
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, (int, float)):
        return str(value)
    if isinstance(value, str):
        return json.dumps(value)
    if isinstance(value, list):
        return "[" + ", ".join(toml_value(v) for v in value) + "]"
    if isinstance(value, dict):
        return "{ " + ", ".join(f"{k} = {toml_value(v)}" for k, v in value.items()) + " }"
    raise TypeError(value)


def table(name, body):
    return f"[{name}]\n" + "".join(f"{k} = {toml_value(v)}\n" for k, v in body.items()) + "\n"


def array_table(name, rows):
    return "".join(f"[[{name}]]\n" + "".join(f"{k} = {toml_value(v)}\n" for k, v in row.items()) + "\n" for row in rows)


def pools_for(tokens, crosses):
    pools = []
    for token in tokens:
        if token == "USDC":
            continue
        for venue in ("manifest", "orca-whirlpool", "raydium-clmm"):
            pools.append({"venue": venue, "base": token, "quote": "USDC"})
    for base, quote in crosses:
        pools.append({"venue": "orca-whirlpool", "base": base, "quote": quote})
    return pools


def makers_for(pools, density, prices, capital, new_tokens=()):
    makers = []
    for pool in pools:
        base, quote, venue = pool["base"], pool["quote"], pool["venue"]
        pb, pq = prices[base], prices[quote]
        tag = f"{venue.split('-')[0]}-{base.lower()}" + ("" if quote == "USDC" else f"-{quote.lower()}")
        d = 1 if base in new_tokens else density
        for k in range(d):
            if venue == "manifest":
                spread, step, level_usd, refresh = [(10, 8, 560, 5), (25, 15, 900, 10), (50, 30, 400, 15)][k]
                strategy = {"kind": "clob-ladder", "levels": [5, 3, 4][k], "spread_bps": spread, "step_bps": step,
                            "level_size": amount(level_usd * capital, pb), "refresh_bps": refresh}
                if k == 1:
                    strategy |= {"skew_bps": 60, "target_base_ratio": "0.5"}
                makers.append({"name": f"clob-{base.lower()}-{k}", "venue": venue, "base": base, "quote": quote,
                               "interval_ms": 2000 + 500 * k,
                               "inventory": {base: amount(22000 * capital, pb), quote: amount(20000 * capital, pq)}, "strategy": strategy})
            else:
                width, recenter, position = [(600, 200, 20000), (1200, 400, 15000), (2500, 800, 10000)][k]
                makers.append({"name": f"lp-{tag}-{k}", "venue": venue, "base": base, "quote": quote, "interval_ms": 5000,
                               "inventory": {base: amount(position * Decimal("0.8") * capital, pb),
                                             quote: amount(position * Decimal("0.8") * capital, pq)},
                               "strategy": {"kind": "clmm-range", "width_bps": width, "recenter_bps": recenter,
                                            "position_usd": str(position * capital)}})
                threshold = [10, 15, 20][k] + (5 if quote != "USDC" else 0)
                makers.append({"name": f"arb-{tag}-{k}", "venue": venue, "base": base, "quote": quote,
                               "interval_ms": [2000, 2500, 3000][k],
                               "inventory": {base: amount(11000 * capital, pb), quote: amount(11000 * capital, pq)},
                               "strategy": {"kind": "pool-arb", "threshold_bps": threshold,
                                            "max_trade_usd": str([3000, 2000, 1500][k] * capital)}})
    return makers


# Per-bot searcher profile, so the population does not act in lockstep:
# priority fee (lamports per cycle) and reaction latency (ms).
BOT_PROFILES = [(0, 0), (5000, 150), (20000, 400), (0, 50), (10000, 250)]


def bots_for(n, tokens, prices):
    params = [("0.50", "5000", 3), ("0.25", "2500", 3), ("1.00", "10000", 3), ("2.00", "1500", 2), ("0.75", "4000", 3)]
    non_usdc = [t for t in tokens if t != "USDC"]
    bots = []
    for k in range(n):
        min_profit, max_trade, legs = params[k % len(params)]
        fee, latency = BOT_PROFILES[k % len(BOT_PROFILES)]
        starts = ["USDC"] + [non_usdc[(k + j) % len(non_usdc)] for j in range(k % 3)]
        bots.append({"name": f"searcher-{k}", "start_tokens": starts, "max_legs": legs, "min_profit_usd": min_profit,
                     "max_trade_usd": max_trade, "leg_slippage_bps": 5,
                     "inventory": {t: amount(20000, prices[t]) for t in starts}, "sol": "3",
                     "priority_fee_lamports": fee, "latency_ms": latency})
    return bots


def flow_for(takers, tokens, pools, prices, sol):
    pairs = [[p["base"], p["quote"]] for p in pools if p["venue"] == "orca-whirlpool"]
    # Non-USDC pairs without a pool exercise the router's USDC bridge.
    extra = [t for t in tokens if t not in ("USDC", "JUP", "RAY")]
    pairs += [[extra[i], extra[i + 1]] for i in range(0, len(extra) - 1, 2)]
    inventory = {"JUP": "60000", "RAY": "8000", "USDC": "25000"}
    inventory |= {t: amount(5000, prices[t]) for t in extra}
    return {"takers": takers, "rate_per_min": PER_TAKER_RATE * takers, "median_usd": 150.0, "sigma": 1.0,
            "large_probability": 0.08, "large_multiplier": 40.0, "exact_out_probability": 0.1, "slippage_bps": 150,
            "pairs": pairs, "inventory": inventory, "sol": sol, "refill_below_ratio": "0.25", "rng_seed": 7}


def overlay(name, takers, bots, density, capital, tokens, crosses, prices, new_tokens=()):
    pools = pools_for(tokens, crosses)
    makers = makers_for(pools, density, prices, capital, new_tokens)
    sol = "2" if takers < 500 else "0.5"
    text = (f"# Generated by bootstrap/scenario/scale.py — do not edit by hand.\n"
            f"# {name}: {takers} takers, {bots} arb bots, {len(makers)} makers, {len(tokens)} assets, {len(pools)} pools\n"
            f"# ({takers + bots + len(makers)} participants); maker capital ×{capital}. Layer: MTM_PROFILE=scenario-local,{name}\n\n")
    if new_tokens:
        # Thin Binance books (e.g. RENDER) can go >30 s without a top-of-book change.
        text += table("scenario", {"max_price_age_ms": 120000})
        text += array_table("oracle.underlyings", [{"symbol": f"{t}/USDT", "binance_symbol": f"{t}USDT"} for t in tokens if t != "USDC"])
        text += array_table("oracle.instruments", [{"symbol": f"{t}/USD", "base": {"kind": "underlying", "feed": f"{t}/USDT"}} for t in tokens if t != "USDC"]
                            + [{"symbol": "USDC/USD", "base": {"kind": "peg", "target": "1.0000"}}])
        text += array_table("scenario.tokens", [{"name": t, "mint_env": f"PUBKEY_TOKEN_{t}_MINT", "oracle_symbol": f"{t}/USD"} for t in tokens])
    text += array_table("scenario.pools", pools)
    text += array_table("scenario.makers", makers)
    text += table("scenario.flow", flow_for(takers, tokens, pools, prices, sol))
    text += array_table("scenario.arb.bots", bots_for(bots, tokens, prices))
    (ROOT / "config" / f"{name}.toml").write_text(text)
    print(f"{name}: {takers} takers + {bots} bots + {len(makers)} makers = {takers + bots + len(makers)} participants; {len(tokens)} assets, {len(pools)} pools")


def main():
    tokens = BASE_TOKENS + list(NEW_TOKENS)
    only = set(sys.argv[1:])
    prices = {t: price(t) for t in (tokens if not only or "scale-assets" in only else BASE_TOKENS)}
    base_crosses = [("JUP", "RAY")]
    crosses = base_crosses + [(t, "JUP") for t in NEW_TOKENS]
    overlays = {
        "scale-x10": (140, 6, 2, 3, BASE_TOKENS, base_crosses),
        "scale-x100": (1650, 14, 3, 10, BASE_TOKENS, base_crosses),
        "scale-assets": (1650, 14, 3, 10, tokens, crosses),
        "scale-x200": (3300, 20, 3, 20, BASE_TOKENS, base_crosses),
        "scale-x500": (8250, 30, 3, 50, BASE_TOKENS, base_crosses),
        "scale-x750": (12375, 35, 3, 75, BASE_TOKENS, base_crosses),
        "scale-x1000": (16500, 40, 3, 100, BASE_TOKENS, base_crosses),
        "scale-x1500": (24750, 45, 3, 150, BASE_TOKENS, base_crosses),
        "scale-x2000": (33000, 50, 3, 200, BASE_TOKENS, base_crosses),
    }
    for name, args in overlays.items():
        if not only or name in only:
            overlay(name, *args, prices, new_tokens=tuple(NEW_TOKENS) if name == "scale-assets" else ())
    if only and "scale-assets" not in only:
        return
    config = [{"mainnetPubkey": MAINNET[t], "mintAuthority": "KEYPAIR_TOKEN_MINT_AUTHORITY",
               "freezeAuthority": "KEYPAIR_TOKEN_FREEZE_AUTHORITY", "metadataAuthority": "KEYPAIR_TOKEN_METADATA_AUTHORITY",
               "mint": f"KEYPAIR_TOKEN_{t}_MINT"} for t in tokens]
    (ROOT / "bootstrap" / "scenario" / "tokens-assets.json").write_text(json.dumps(config, indent=2) + "\n")


if __name__ == "__main__":
    sys.exit(main())
