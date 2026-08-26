use mtm_common::Symbol;
use oracle_client::{Price, PriceData, PricePoint, PriceSource};

use super::*;

fn instruments(json: &str) -> Vec<InstrumentConfig> {
    serde_json::from_str(json).expect("test instrument config parses")
}

fn sol() -> Symbol {
    Symbol::new("SOL/USD")
}

fn tick(price_mantissa: i128, publish_us: i64) -> PricePoint {
    PricePoint {
        symbol: sol(),
        data: PriceData::Aggregate {
            price: Price::new(price_mantissa, -8),
            conf: Some(Price::new(price_mantissa / 1000, -8)),
            ema: None,
        },
        publish_time_us: Some(publish_us),
        received_at_us: publish_us,
        slot: Some(1),
        source: PriceSource::PythHermes,
    }
}

#[test]
fn passthrough_keeps_source_and_scale_derives() {
    let cfg = instruments(
        r#"[
        {"symbol": "tSOL/USD", "base": {"kind": "underlying", "feed": "SOL/USD"}},
        {"symbol": "tSOLHALF/USD",
         "base": {"kind": "underlying", "feed": "SOL/USD"},
         "transforms": [{"kind": "scale", "factor": "0.5"}]}
    ]"#,
    );
    let mut engine = Engine::from_config(&cfg, &[sol()]).expect("builds");
    let out = engine.on_underlying(&tick(9_700_000_000, 1_000_000), 1_000_000);
    assert_eq!(out.len(), 2);

    let pass = out
        .iter()
        .find(|p| p.symbol.as_str() == "tSOL/USD")
        .expect("passthrough");
    assert_eq!(pass.source, PriceSource::PythHermes);
    let PriceData::Aggregate { price, conf, .. } = &pass.data else {
        panic!("aggregate")
    };
    assert_eq!(*price, Price::new(9_700_000_000, -8));
    assert_eq!(*conf, Some(Price::new(9_700_000, -8)));

    let half = out
        .iter()
        .find(|p| p.symbol.as_str() == "tSOLHALF/USD")
        .expect("scaled");
    assert_eq!(half.source, PriceSource::Derived);
    let PriceData::Aggregate { price, .. } = &half.data else {
        panic!("aggregate")
    };
    assert_eq!(*price, Price::new(4_850_000_000, -8));
}

#[test]
fn mint_address_symbol_survives() {
    let cfg = instruments(
        r#"[{"mint": "7pMcAg9x3GJqUxWZcntjZiy5UJPXfPZFoVwuCPCBpMcx",
             "base": {"kind": "underlying", "feed": "SOL/USD"}}]"#,
    );
    let mut engine = Engine::from_config(&cfg, &[sol()]).expect("builds");
    let out = engine.on_underlying(&tick(100, 1), 1);
    assert_eq!(
        out[0].symbol.as_str(),
        "7pMcAg9x3GJqUxWZcntjZiy5UJPXfPZFoVwuCPCBpMcx"
    );
}

#[test]
fn lag_serves_old_values() {
    let cfg = instruments(
        r#"[{"symbol": "tLAG/USD",
             "base": {"kind": "underlying", "feed": "SOL/USD"},
             "transforms": [{"kind": "lag", "ms": 10}]}]"#,
    );
    let mut engine = Engine::from_config(&cfg, &[sol()]).expect("builds");

    // t=0: nothing old enough yet
    assert!(engine.on_underlying(&tick(100, 0), 0).is_empty());
    // t=5ms: still nothing
    assert!(engine.on_underlying(&tick(200, 5_000), 5_000).is_empty());
    // t=12ms: the t=0 sample (value 100) is now 12ms old — served
    let out = engine.on_underlying(&tick(300, 12_000), 12_000);
    assert_eq!(out.len(), 1);
    let PriceData::Aggregate { price, .. } = &out[0].data else {
        panic!("aggregate")
    };
    assert_eq!(price.mantissa, 100);
    assert_eq!(out[0].source, PriceSource::Derived);
    // and it carries the lagged sample's publish time
    assert_eq!(out[0].publish_time_us, Some(0));
}

#[test]
fn seeded_world_is_reproducible() {
    let json = r#"[
        {"symbol": "tNOISY/USD",
         "base": {"kind": "underlying", "feed": "SOL/USD"},
         "transforms": [{"kind": "noise", "sigma_bps": 20, "halflife_s": 60, "seed": 7}]},
        {"symbol": "tMEME/USD",
         "base": {"kind": "gbm", "initial": "0.02", "daily_vol_bps": 1500, "seed": 42, "tick_ms": 250}}
    ]"#;
    let run = || {
        let mut engine = Engine::from_config(&instruments(json), &[sol()]).expect("builds");
        let mut points = Vec::new();
        for step in 0..50i64 {
            let now = step * 250_000;
            points.extend(engine.on_underlying(&tick(9_700_000_000 + step as i128, now), now));
            points.extend(engine.on_tick(now));
        }
        points
            .iter()
            .map(|p| {
                let PriceData::Aggregate { price, .. } = &p.data else {
                    panic!("aggregate")
                };
                (p.symbol.as_str().to_string(), price.mantissa)
            })
            .collect::<Vec<_>>()
    };
    let a = run();
    let b = run();
    assert_eq!(
        a, b,
        "same config + same tape must reproduce identical prices"
    );
    assert!(
        a.iter().any(|(s, _)| s == "tMEME/USD"),
        "gbm produced points"
    );
    // noise actually perturbs: noisy != raw underlying somewhere
    assert!(
        a.iter()
            .filter(|(s, _)| s == "tNOISY/USD")
            .any(|(_, m)| *m != 9_700_000_000),
        "noise should move the price"
    );
}

#[test]
fn basket_rebases_and_peg_holds() {
    let btc = Symbol::new("BTC/USD");
    let cfg = instruments(
        r#"[
        {"symbol": "tIDX/USD", "base": {"kind": "basket", "rebase": "100",
            "legs": [{"feed": "SOL/USD", "weight": "0.5"}, {"feed": "BTC/USD", "weight": "0.5"}]}},
        {"symbol": "tUSD/USD", "base": {"kind": "peg", "target": "1.0"}}
    ]"#,
    );
    let mut engine = Engine::from_config(&cfg, &[sol(), btc.clone()]).expect("builds");

    // only SOL known → basket can't evaluate yet
    assert!(engine.on_underlying(&tick(9_700_000_000, 1), 1).is_empty());
    let mut btc_tick = tick(7_900_000_000_000, 2);
    btc_tick.symbol = btc;
    let out = engine.on_underlying(&btc_tick, 2);
    let idx = out
        .iter()
        .find(|p| p.symbol.as_str() == "tIDX/USD")
        .expect("basket");
    let PriceData::Aggregate { price, .. } = &idx.data else {
        panic!("aggregate")
    };
    // first evaluation rebases to exactly 100
    assert_eq!(*price, Price::new(10_000_000_000, -8));

    let pegs = engine.on_tick(10);
    let peg = pegs
        .iter()
        .find(|p| p.symbol.as_str() == "tUSD/USD")
        .expect("peg");
    let PriceData::Aggregate { price, .. } = &peg.data else {
        panic!("aggregate")
    };
    assert_eq!(*price, Price::new(100_000_000, -8));
}

#[test]
fn top_of_book_reduces_to_mid_with_half_spread_conf() {
    let cfg = instruments(
        r#"[{"symbol": "tBN/USD", "base": {"kind": "underlying", "feed": "SOL/USDT"}}]"#,
    );
    let feed = Symbol::new("SOL/USDT");
    let mut engine = Engine::from_config(&cfg, std::slice::from_ref(&feed)).expect("builds");
    let point = PricePoint {
        symbol: feed,
        data: PriceData::TopOfBook {
            bid: Price::new(9_675_000_000, -8), // 96.75
            bid_qty: Price::new(10, 0),
            ask: Price::new(9_677_000_000, -8), // 96.77
            ask_qty: Price::new(5, 0),
        },
        publish_time_us: None,
        received_at_us: 1_000,
        slot: None,
        source: PriceSource::Binance,
    };
    let out = engine.on_underlying(&point, 1_000);
    assert_eq!(out.len(), 1);
    let PriceData::Aggregate { price, conf, .. } = &out[0].data else {
        panic!("aggregate")
    };
    assert_eq!(*price, Price::new(9_676_000_000, -8)); // mid = 96.76
    assert_eq!(*conf, Some(Price::new(1_000_000, -8))); // half-spread = 0.01
    assert_eq!(out[0].source, PriceSource::Binance); // passthrough keeps source
    assert_eq!(out[0].publish_time_us, None); // untimestamped stays honest
}

#[test]
fn unknown_feed_and_duplicate_symbol_rejected() {
    let missing = instruments(
        r#"[{"symbol": "tX/USD", "base": {"kind": "underlying", "feed": "NOPE/USD"}}]"#,
    );
    assert!(Engine::from_config(&missing, &[sol()]).is_err());

    let dupes = instruments(
        r#"[
        {"symbol": "tX/USD", "base": {"kind": "underlying", "feed": "SOL/USD"}},
        {"symbol": "tX/USD", "base": {"kind": "underlying", "feed": "SOL/USD"}}
    ]"#,
    );
    assert!(Engine::from_config(&dupes, &[sol()]).is_err());
}
