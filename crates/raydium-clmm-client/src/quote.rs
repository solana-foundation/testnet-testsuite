use std::collections::VecDeque;

use raydium_clmm_interface::libraries::{liquidity_math, swap_math, tick_math};
use raydium_clmm_interface::states::{
    AmmConfig, PoolState, TickArrayBitmapExtension, TickArrayState,
};

struct SwapState {
    remaining: u64,
    calculated: u64,
    sqrt_price_x64: u128,
    tick: i32,
    liquidity: u128,
}

pub(crate) fn quote_swap(
    amount_specified: u64,
    zero_for_one: bool,
    exact_in: bool,
    config: &AmmConfig,
    pool: &PoolState,
    bitmap: &TickArrayBitmapExtension,
    tick_arrays: &mut VecDeque<TickArrayState>,
) -> Result<(u64, Vec<i32>), String> {
    if amount_specified == 0 {
        return Err("swap amount must be greater than zero".to_owned());
    }
    let sqrt_price_limit_x64 = if zero_for_one {
        tick_math::MIN_SQRT_PRICE_X64 + 1
    } else {
        tick_math::MAX_SQRT_PRICE_X64 - 1
    };
    let (mut matches_current, mut current_start) = pool
        .get_first_initialized_tick_array(&Some(*bitmap), zero_for_one)
        .map_err(|error| error.to_string())?;

    let mut current_array = tick_arrays
        .pop_front()
        .ok_or_else(|| "no initialized tick array is available".to_owned())?;
    if current_array.start_tick_index != current_start {
        return Err("tick arrays were not supplied in traversal order".to_owned());
    }
    let mut used = vec![current_start];
    let mut state = SwapState {
        remaining: amount_specified,
        calculated: 0,
        sqrt_price_x64: pool.sqrt_price_x64,
        tick: pool.tick_current,
        liquidity: pool.liquidity,
    };

    // A legacy transaction cannot carry an unbounded number of arrays. The
    // caller separately enforces the stricter account-budget cap.
    for _ in 0..64 {
        if state.remaining == 0
            || state.sqrt_price_x64 == sqrt_price_limit_x64
            || state.tick >= tick_math::MAX_TICK
            || state.tick <= tick_math::MIN_TICK
        {
            return Ok((state.calculated, used));
        }

        let mut next_tick = current_array
            .next_initialized_tick(state.tick, pool.tick_spacing, zero_for_one)
            .map_err(|error| error.to_string())?
            .copied()
            .unwrap_or_default();
        if !next_tick.is_initialized() && !matches_current {
            matches_current = true;
            next_tick = *current_array
                .first_initialized_tick(zero_for_one)
                .map_err(|error| error.to_string())?;
        }
        if !next_tick.is_initialized() {
            let next_start = pool
                .next_initialized_tick_array_start_index(
                    &Some(*bitmap),
                    current_start,
                    zero_for_one,
                )
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "swap exhausts initialized tick arrays".to_owned())?;
            current_array = tick_arrays
                .pop_front()
                .ok_or_else(|| "quote requires another tick array".to_owned())?;
            if current_array.start_tick_index != next_start {
                return Err("tick arrays were not supplied in traversal order".to_owned());
            }
            current_start = next_start;
            used.push(next_start);
            next_tick = *current_array
                .first_initialized_tick(zero_for_one)
                .map_err(|error| error.to_string())?;
            if !next_tick.is_initialized() {
                return Err("initialized tick array contains no initialized tick".to_owned());
            }
        }
        if next_tick.has_limit_orders() {
            return Err(
                "limit-order liquidity is outside this single-pool quote implementation".to_owned(),
            );
        }

        let tick_next = next_tick
            .tick
            .clamp(tick_math::MIN_TICK, tick_math::MAX_TICK);
        let sqrt_price_next_x64 =
            tick_math::get_sqrt_price_at_tick(tick_next).map_err(|error| error.to_string())?;
        let target = if (zero_for_one && sqrt_price_next_x64 < sqrt_price_limit_x64)
            || (!zero_for_one && sqrt_price_next_x64 > sqrt_price_limit_x64)
        {
            sqrt_price_limit_x64
        } else {
            sqrt_price_next_x64
        };
        let step = swap_math::compute_swap(
            state.sqrt_price_x64,
            target,
            state.liquidity,
            state.remaining,
            config.trade_fee_rate,
            exact_in,
            zero_for_one,
            pool.is_fee_on_input(zero_for_one),
        )
        .map_err(|error| error.to_string())?;
        let step_total_in = step
            .amount_in
            .checked_add(step.fee_amount)
            .ok_or_else(|| "swap input overflow".to_owned())?;
        if exact_in {
            state.remaining = state
                .remaining
                .checked_sub(step_total_in)
                .ok_or_else(|| "swap input underflow".to_owned())?;
            state.calculated = state
                .calculated
                .checked_add(step.amount_out)
                .ok_or_else(|| "swap output overflow".to_owned())?;
        } else {
            state.remaining = state
                .remaining
                .checked_sub(step.amount_out)
                .ok_or_else(|| "swap output underflow".to_owned())?;
            state.calculated = state
                .calculated
                .checked_add(step_total_in)
                .ok_or_else(|| "swap input overflow".to_owned())?;
        }

        let prior_sqrt_price = state.sqrt_price_x64;
        state.sqrt_price_x64 = step.sqrt_price_next_x64;
        if state.sqrt_price_x64 == sqrt_price_next_x64 {
            if next_tick.is_initialized() {
                let liquidity_net = if zero_for_one {
                    next_tick
                        .liquidity_net
                        .checked_neg()
                        .ok_or_else(|| "liquidity delta overflow".to_owned())?
                } else {
                    next_tick.liquidity_net
                };
                state.liquidity = liquidity_math::add_delta(state.liquidity, liquidity_net)
                    .map_err(|error| error.to_string())?;
            }
            state.tick = if zero_for_one {
                tick_next
                    .checked_sub(1)
                    .ok_or_else(|| "tick underflow".to_owned())?
            } else {
                tick_next
            };
        } else if state.sqrt_price_x64 != prior_sqrt_price {
            state.tick = tick_math::get_tick_at_sqrt_price(state.sqrt_price_x64)
                .map_err(|error| error.to_string())?;
        }
    }
    Err("swap quote exceeded the traversal limit".to_owned())
}
