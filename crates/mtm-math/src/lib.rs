//! Fixed-point price/size math shared across services.
//!
//! Money is never represented as `f64`. The canonical [`Price`] is
//! `mantissa * 10^expo` with an i128 mantissa — wide enough for every source
//! we ingest (Pyth is i64, but Switchboard stores i128 at a fixed scale of 18,
//! which overflows i64 for any asset above ~$9). Design rationale and the full
//! source survey live in docs/pricing-types.md.

use std::fmt;
use std::str::FromStr;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MathError {
    #[error("arithmetic overflow")]
    Overflow,
    #[error("invalid price string: {0}")]
    Parse(String),
}

/// Canonical fixed-point price: `mantissa * 10^expo`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Price {
    #[serde(with = "mantissa_serde")]
    pub mantissa: i128,
    pub expo: i32,
}

impl Price {
    pub const fn new(mantissa: i128, expo: i32) -> Self {
        Self { mantissa, expo }
    }

    pub const fn from_pyth(mantissa: i64, expo: i32) -> Self {
        Self::new(mantissa as i128, expo)
    }

    pub fn to_pyth(&self) -> Option<(i64, i32)> {
        i64::try_from(self.mantissa).ok().map(|m| (m, self.expo))
    }

    pub fn to_f64(&self) -> f64 {
        self.mantissa as f64 * 10f64.powi(self.expo)
    }

    pub fn to_decimal(&self) -> Option<Decimal> {
        if self.expo <= 0 {
            let scale = u32::try_from(i64::from(self.expo).unsigned_abs()).ok()?;
            Decimal::try_from_i128_with_scale(self.mantissa, scale).ok()
        } else {
            let mut d = Decimal::try_from_i128_with_scale(self.mantissa, 0).ok()?;
            for _ in 0..self.expo {
                d = d.checked_mul(Decimal::TEN)?;
            }
            Some(d)
        }
    }

    pub fn from_decimal(value: Decimal, expo: i32) -> Result<Price, MathError> {
        use rust_decimal::prelude::ToPrimitive;
        let factor = pow10(expo.unsigned_abs())
            .and_then(|f| Decimal::try_from_i128_with_scale(f, 0).ok())
            .ok_or(MathError::Overflow)?;
        let shifted = if expo <= 0 {
            value.checked_mul(factor).ok_or(MathError::Overflow)?
        } else {
            value.checked_div(factor).ok_or(MathError::Overflow)?
        };
        let mantissa = shifted.round().to_i128().ok_or(MathError::Overflow)?;
        Ok(Price::new(mantissa, expo))
    }

    pub fn from_f64_lossy(value: f64, expo: i32) -> Result<Price, MathError> {
        if !value.is_finite() {
            return Err(MathError::Parse(value.to_string()));
        }
        let scaled = value * 10f64.powi(-expo);
        if !scaled.is_finite() || scaled.abs() >= i128::MAX as f64 {
            return Err(MathError::Overflow);
        }
        Ok(Price::new(scaled.round() as i128, expo))
    }

    pub fn checked_scale(self, new_expo: i32) -> Result<Price, MathError> {
        let diff = self.expo - new_expo;
        if diff == 0 {
            return Ok(self);
        }
        let factor = pow10(diff.unsigned_abs()).ok_or(MathError::Overflow)?;
        if diff > 0 {
            let mantissa = self
                .mantissa
                .checked_mul(factor)
                .ok_or(MathError::Overflow)?;
            Ok(Price::new(mantissa, new_expo))
        } else {
            Ok(Price::new(self.mantissa / factor, new_expo))
        }
    }

    pub fn apply_bps(self, bps: i64) -> Result<Price, MathError> {
        let scaled = self
            .mantissa
            .checked_mul(
                10_000i128
                    .checked_add(bps as i128)
                    .ok_or(MathError::Overflow)?,
            )
            .ok_or(MathError::Overflow)?;
        Ok(Price::new(scaled / 10_000, self.expo))
    }
}

fn pow10(exp: u32) -> Option<i128> {
    10i128.checked_pow(exp)
}

impl fmt::Display for Price {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.expo >= 0 {
            match pow10(self.expo as u32).and_then(|s| self.mantissa.checked_mul(s)) {
                Some(v) => write!(f, "{v}"),
                None => write!(f, "{}e{}", self.mantissa, self.expo),
            }
        } else {
            let Some(scale) = pow10(self.expo.unsigned_abs()) else {
                return write!(f, "{}e{}", self.mantissa, self.expo);
            };
            let (sign, abs) = if self.mantissa < 0 {
                ("-", self.mantissa.unsigned_abs())
            } else {
                ("", self.mantissa.unsigned_abs())
            };
            let scale = scale as u128;
            let width = self.expo.unsigned_abs() as usize;
            write!(f, "{sign}{}.{:0width$}", abs / scale, abs % scale)
        }
    }
}

impl FromStr for Price {
    type Err = MathError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (int_part, frac_part) = s.split_once('.').unwrap_or((s, ""));
        let mantissa: i128 = format!("{int_part}{frac_part}")
            .parse()
            .map_err(|_| MathError::Parse(s.to_string()))?;
        let expo = i32::try_from(frac_part.len()).map_err(|_| MathError::Parse(s.to_string()))?;
        Ok(Price::new(mantissa, -expo))
    }
}

/// Mantissa on the wire: serialize as decimal string, accept string or integer.
mod mantissa_serde {
    use std::fmt;

    use serde::de::{self, Visitor};
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &i128, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(v)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<i128, D::Error> {
        struct MantissaVisitor;

        impl Visitor<'_> for MantissaVisitor {
            type Value = i128;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an integer mantissa as a decimal string or number")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<i128, E> {
                v.parse().map_err(|_| E::custom("invalid mantissa string"))
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<i128, E> {
                Ok(v.into())
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<i128, E> {
                Ok(v.into())
            }

            fn visit_i128<E: de::Error>(self, v: i128) -> Result<i128, E> {
                Ok(v)
            }
        }

        d.deserialize_any(MantissaVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_display_roundtrip() {
        let p: Price = "153.27".parse().expect("parses");
        assert_eq!(p, Price::new(15327, -2));
        assert_eq!(p.to_string(), "153.27");

        let n: Price = "-0.05".parse().expect("parses");
        assert_eq!(n, Price::new(-5, -2));
        assert_eq!(n.to_string(), "-0.05");

        assert_eq!(Price::new(105, -4).to_string(), "0.0105");
        assert_eq!(Price::new(42, 0).to_string(), "42");
    }

    #[test]
    fn holds_switchboard_scale_18() {
        // SOL at $97 in Switchboard's fixed scale-18: overflows i64, fits i128
        let p = Price::new(97_000_000_000_000_000_000, -18);
        assert_eq!(p.to_string(), "97.000000000000000000");
        assert_eq!(p.to_pyth(), None); // doesn't fit i64 as-is
        let rescaled = p.checked_scale(-8).expect("rescales");
        assert_eq!(rescaled.to_pyth(), Some((9_700_000_000, -8)));
    }

    #[test]
    fn scaling_and_bps() {
        let p = Price::new(15327, -2);
        assert_eq!(p.checked_scale(-4), Ok(Price::new(1_532_700, -4)));
        assert_eq!(p.checked_scale(-1), Ok(Price::new(1532, -1))); // truncates

        let p = Price::new(1_000_000, -4); // 100.0000
        assert_eq!(p.apply_bps(10), Ok(Price::new(1_001_000, -4)));
        assert_eq!(p.apply_bps(-10), Ok(Price::new(999_000, -4)));
    }

    #[test]
    fn quantization() {
        let d = Decimal::new(97_558_680_415, 9); // 97.558680415
        assert_eq!(
            Price::from_decimal(d, -8),
            Ok(Price::new(9_755_868_042, -8)) // banker's rounding on the last digit
        );
        assert_eq!(
            Price::from_f64_lossy(97.55868041, -8),
            Ok(Price::new(9_755_868_041, -8))
        );
        assert!(Price::from_f64_lossy(f64::NAN, -8).is_err());
        assert!(Price::from_f64_lossy(f64::INFINITY, -8).is_err());
    }

    #[test]
    fn decimal_conversion() {
        let p = Price::new(9_755_868_041, -8);
        assert_eq!(p.to_decimal(), Some(Decimal::new(9_755_868_041, 8)));
        let big = Price::new(97_000_000_000_000_000_000, -18);
        assert_eq!(
            big.to_decimal().map(|d| d.to_string()).as_deref(),
            Some("97.000000000000000000")
        );
    }

    #[test]
    fn serde_mantissa_is_string_but_accepts_int() {
        let p = Price::new(9_755_868_041, -8);
        let json = serde_json::to_string(&p).expect("serializes");
        assert_eq!(json, r#"{"mantissa":"9755868041","expo":-8}"#);
        let back: Price = serde_json::from_str(&json).expect("roundtrips");
        assert_eq!(back, p);
        let from_int: Price =
            serde_json::from_str(r#"{"mantissa":9755868041,"expo":-8}"#).expect("accepts int");
        assert_eq!(from_int, p);
    }
}
