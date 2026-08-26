//! Time helpers. All internal timestamps are unix **microseconds** (i64) —
//! see docs/pricing-types.md for why (source APIs span s/ms/µs/RFC3339).

use std::time::{SystemTime, UNIX_EPOCH};

/// Current unix time in microseconds.
pub fn now_us() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}
