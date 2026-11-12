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

#[tokio::test]
async fn clock_driven_instruments_outlive_a_closed_feed_channel() {
    let cfg = instruments(r#"[{"symbol": "tUSD/USD", "base": {"kind": "peg", "target": "1.0"}}]"#);
    let engine = Engine::from_config(&cfg, &[]).expect("builds");
    let state = AppState::new(
        Vec::new(),
        crate::state::ReadyLimits::uniform(std::time::Duration::from_secs(15)),
    );
    // No feeds: the input is closed before the engine starts.
    let (tx, rx) = mpsc::channel(1);
    drop(tx);
    let task = tokio::spawn(run(engine, rx, state.clone(), None));
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    assert!(
        !task.is_finished(),
        "engine stopped on a closed feed channel"
    );
    assert!(state.get(&Symbol::new("tUSD/USD")).await.is_some());
    task.abort();
}

fn mantissas(points: &[PricePoint]) -> Vec<(String, i128)> {
    points
        .iter()
        .map(|p| {
            let PriceData::Aggregate { price, .. } = &p.data else {
                panic!("aggregate")
            };
            (p.symbol.as_str().to_string(), price.mantissa)
        })
        .collect()
}

const RESTART_JSON: &str = r#"[
    {"symbol": "tMEME/USD",
     "base": {"kind": "gbm", "initial": "0.02", "daily_vol_bps": 1500, "seed": 42, "tick_ms": 250}},
    {"symbol": "tIDX/USD", "base": {"kind": "basket", "rebase": "100",
        "legs": [{"feed": "SOL/USD", "weight": "1"}]}}
]"#;

#[test]
fn a_restart_carries_on_from_the_saved_state() {
    let cfg = instruments(RESTART_JSON);
    let mut before = Engine::from_config(&cfg, &[sol()]).expect("builds");
    // Walk the GBM and rebase the basket at SOL = 97.
    for step in 0..20i64 {
        before.on_tick(step * 250_000);
    }
    before.on_underlying(&tick(9_700_000_000, 1), 1);
    let saved = before.saved_state();
    assert_eq!(saved.instruments.len(), 2);

    // Through the file, as a restart would.
    let dir = std::env::temp_dir().join(format!("oracle-state-{}", std::process::id()));
    let path = dir.join("pricing.json");
    save_state(&path, &saved).expect("saves");
    let loaded = load_state(&path).expect("loads");
    assert_eq!(loaded, saved);
    let _ = std::fs::remove_dir_all(&dir);

    let mut after = Engine::from_config(&cfg, &[sol()]).expect("builds");
    assert_eq!(after.restore(&loaded), 2);

    // The next GBM step and the basket at a new SOL price are what the
    // engine that kept running would have produced (no jump to `initial`,
    // no rebase to 100 at the new SOL price).
    let now = 20 * 250_000;
    assert_eq!(
        mantissas(&after.on_tick(now)),
        mantissas(&before.on_tick(now))
    );
    let moved = tick(10_670_000_000, 2);
    let a = mantissas(&after.on_underlying(&moved, 2));
    assert_eq!(a, mantissas(&before.on_underlying(&moved, 2)));
    assert_eq!(a, vec![("tIDX/USD".to_string(), 11_000_000_000)]);

    // Without the restore, a fresh engine re-anchors: the check above is
    // not vacuous.
    let mut fresh = Engine::from_config(&cfg, &[sol()]).expect("builds");
    assert_eq!(
        mantissas(&fresh.on_underlying(&moved, 2)),
        vec![("tIDX/USD".to_string(), 10_000_000_000)]
    );
}

#[test]
fn saved_state_for_a_changed_config_is_not_restored() {
    let mut before = Engine::from_config(&instruments(RESTART_JSON), &[sol()]).expect("builds");
    before.on_tick(0);
    let saved = before.saved_state();
    let changed = RESTART_JSON.replace(r#""initial": "0.02""#, r#""initial": "0.05""#);
    let mut after = Engine::from_config(&instruments(&changed), &[sol()]).expect("builds");
    // The basket has no factor yet, so only the GBM was saved; its config
    // changed, so nothing is restored.
    assert_eq!(after.restore(&saved), 0);
    let out = mantissas(&after.on_tick(0));
    let mut fresh = Engine::from_config(&instruments(&changed), &[sol()]).expect("builds");
    assert_eq!(out, mantissas(&fresh.on_tick(0)));
}

#[test]
fn a_missing_or_corrupt_state_file_is_ignored() {
    let dir = std::env::temp_dir().join(format!("oracle-state-bad-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    assert!(load_state(&dir.join("absent.json")).is_none());
    let bad = dir.join("bad.json");
    std::fs::write(&bad, b"{not json").expect("write");
    assert!(load_state(&bad).is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_engine_task_saves_its_state() {
    let cfg = instruments(RESTART_JSON);
    let engine = Engine::from_config(&cfg, &[sol()]).expect("builds");
    let state = AppState::new(
        Vec::new(),
        crate::state::ReadyLimits::uniform(std::time::Duration::from_secs(15)),
    );
    let dir = std::env::temp_dir().join(format!("oracle-state-run-{}", std::process::id()));
    let path = dir.join("pricing.json");
    let (_tx, rx) = mpsc::channel(1);
    let task = tokio::spawn(run(engine, rx, state, Some(path.clone())));
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    task.abort();
    let saved = load_state(&path).expect("saved on the first tick");
    let _ = std::fs::remove_dir_all(&dir);
    let Some(SavedInstrument {
        state: SavedBase::Gbm { .. },
        ..
    }) = saved.instruments.get(&Symbol::new("tMEME/USD"))
    else {
        panic!("gbm state saved: {saved:?}");
    };
}
