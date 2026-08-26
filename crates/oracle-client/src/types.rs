//! Oracle API wire types. Design rationale: docs/pricing-types.md.
//! `Price` lives in mtm-math, `Symbol` in mtm-common; this module owns the
//! observation envelope.

use mtm_common::Symbol;
use mtm_math::Price;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum PriceData {
    Aggregate {
        price: Price,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        conf: Option<Price>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ema: Option<Price>,
    },
    TopOfBook {
        bid: Price,
        bid_qty: Price,
        ask: Price,
        ask_qty: Price,
    },
    Trade { price: Price, qty: Price },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum PriceSource {
    PythHermes,
    PythLazer,
    Binance,
    Coinbase,
    Kraken,
    Okx,
    #[serde(rename = "coingecko")]
    CoinGecko,
    Derived,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstrumentInfo {
    pub symbol: Symbol,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mint: Option<String>,
    pub base: String,
    #[serde(default)]
    pub transforms: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PricePoint {
    pub symbol: Symbol,
    pub data: PriceData,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish_time_us: Option<i64>,
    pub received_at_us: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<u64>,
    pub source: PriceSource,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_shape() {
        let point = PricePoint {
            symbol: Symbol::new("SOL/USD"),
            data: PriceData::Aggregate {
                price: Price::new(9_755_868_041, -8),
                conf: Some(Price::new(4_306_041, -8)),
                ema: None,
            },
            publish_time_us: Some(1_787_678_802_000_000),
            received_at_us: 1_787_678_802_123_456,
            slot: Some(310_563_706),
            source: PriceSource::PythHermes,
        };
        let json = serde_json::to_value(&point).expect("serializes");
        assert_eq!(json["data"]["kind"], "aggregate");
        assert_eq!(json["data"]["price"]["mantissa"], "9755868041");
        assert_eq!(json["source"], "pyth-hermes");
        assert!(json["data"].get("ema").is_none());
        let back: PricePoint = serde_json::from_value(json).expect("roundtrips");
        assert_eq!(back.slot, Some(310_563_706));
    }
}
