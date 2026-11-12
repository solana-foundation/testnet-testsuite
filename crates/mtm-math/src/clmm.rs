//! Concentrated-liquidity fee accounting shared by the Orca and Raydium
//! clients (both use the Uniswap-v3 fee-growth scheme in Q64.64).

/// Fees a position has earned since its last checkpoint, from the pool's
/// fee growth (Q64.64 per unit of liquidity), as the program computes it on
/// the next position update.
#[allow(clippy::too_many_arguments)]
pub fn fees_accrued(
    liquidity: u128,
    global: u128,
    lower_outside: u128,
    upper_outside: u128,
    current: i32,
    lower: i32,
    upper: i32,
    checkpoint: u128,
) -> u64 {
    let below = if current >= lower {
        lower_outside
    } else {
        global.wrapping_sub(lower_outside)
    };
    let above = if current < upper {
        upper_outside
    } else {
        global.wrapping_sub(upper_outside)
    };
    let inside = global.wrapping_sub(below).wrapping_sub(above);
    let growth = inside.wrapping_sub(checkpoint);
    u64::try_from(mul_shr_64(liquidity, growth)).unwrap_or(u64::MAX)
}

/// `(a × b) >> 64` exactly (saturating), via 64-bit limbs.
pub fn mul_shr_64(a: u128, b: u128) -> u128 {
    let (a_hi, a_lo) = (a >> 64, a & u128::from(u64::MAX));
    let (b_hi, b_lo) = (b >> 64, b & u128::from(u64::MAX));
    let high = a_hi.saturating_mul(b_hi);
    if high > u128::from(u64::MAX) {
        return u128::MAX;
    }
    (high << 64)
        .saturating_add((a_lo * b_lo) >> 64)
        .saturating_add(a_hi * b_lo)
        .saturating_add(a_lo * b_hi)
}

#[cfg(test)]
mod tests {
    use super::*;

    const Q64: u128 = 1 << 64;

    #[test]
    fn mul_shr_64_is_exact() {
        assert_eq!(mul_shr_64(3 * Q64, 5), 15);
        assert_eq!(mul_shr_64(Q64 + 1, Q64 * 2), 2 * Q64 + 2);
        assert_eq!(mul_shr_64(Q64 * 2, Q64 * 2), 4 * Q64);
        assert_eq!(mul_shr_64(u128::MAX, u128::MAX), u128::MAX);
    }

    #[test]
    fn in_range_position_earns_inside_growth() {
        // 1 unit of growth per liquidity inside, position checkpoint at 0.
        let liquidity = 1_000_000;
        let fees = fees_accrued(liquidity, 10 * Q64, 2 * Q64, 3 * Q64, 0, -10, 10, 0);
        assert_eq!(fees, 5 * 1_000_000);
        // Price below the range: inside growth is lower_outside − upper_outside.
        let below = fees_accrued(liquidity, 10 * Q64, 3 * Q64, 2 * Q64, -20, -10, 10, 0);
        assert_eq!(below, 1_000_000);
        // Growth already checkpointed earns nothing.
        assert_eq!(
            fees_accrued(liquidity, 10 * Q64, 2 * Q64, 3 * Q64, 0, -10, 10, 5 * Q64),
            0
        );
    }
}
