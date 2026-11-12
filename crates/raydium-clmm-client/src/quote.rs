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

    let fee_on_input = pool.is_fee_on_input(zero_for_one);
    // A legacy transaction cannot carry an unbounded number of arrays. The
    // caller separately enforces the stricter account-budget cap.
    for _ in 0..64 {
        if state.remaining == 0 {
            return Ok((state.calculated, used));
        }
        // The swap instruction is sent without a price limit, and the program
        // then requires the whole specified amount to fill.
        if state.sqrt_price_x64 == sqrt_price_limit_x64
            || state.tick >= tick_math::MAX_TICK
            || state.tick <= tick_math::MIN_TICK
        {
            return Err(format!(
                "pool liquidity fills only part of the swap ({} of {amount_specified} atoms unfilled)",
                state.remaining
            ));
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
            fee_on_input,
        )
        .map_err(|error| error.to_string())?;
        (state.remaining, state.calculated) = apply_step(
            state.remaining,
            state.calculated,
            (step.amount_in, step.amount_out, step.fee_amount),
            exact_in,
            fee_on_input,
        )?;

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

/// One step's effect on `(remaining, calculated)`, as the program's
/// `SwapState::apply_swap_amounts`: the trader pays the fee on top of
/// `amount_in` only when the fee is charged on the input side; with the fee
/// on the output, `amount_out` is already net of it.
fn apply_step(
    remaining: u64,
    calculated: u64,
    (amount_in, amount_out, fee_amount): (u64, u64, u64),
    exact_in: bool,
    fee_on_input: bool,
) -> Result<(u64, u64), String> {
    let consumed = if fee_on_input {
        amount_in
            .checked_add(fee_amount)
            .ok_or_else(|| "swap input overflow".to_owned())?
    } else {
        amount_in
    };
    let (spent, gained) = if exact_in {
        (consumed, amount_out)
    } else {
        (amount_out, consumed)
    };
    Ok((
        remaining
            .checked_sub(spent)
            .ok_or_else(|| "swap amount underflow".to_owned())?,
        calculated
            .checked_add(gained)
            .ok_or_else(|| "swap amount overflow".to_owned())?,
    ))
}

#[cfg(test)]
mod tests {
    use super::apply_step;

    #[test]
    fn fee_on_input_is_paid_on_top_of_amount_in() {
        // exact in: 1000 remaining, step takes 990 + 10 fee for 500 out.
        assert_eq!(
            apply_step(1000, 0, (990, 500, 10), true, true),
            Ok((0, 500))
        );
        // exact out: 500 wanted, step delivers 500 for 990 + 10 fee in.
        assert_eq!(
            apply_step(500, 0, (990, 500, 10), false, true),
            Ok((0, 1000))
        );
    }

    #[test]
    fn fee_on_output_is_not_deducted_from_the_input() {
        // The fee comes out of the gross output; the trader pays amount_in only.
        assert_eq!(
            apply_step(1000, 0, (1000, 495, 5), true, false),
            Ok((0, 495))
        );
        assert_eq!(
            apply_step(495, 0, (1000, 495, 5), false, false),
            Ok((0, 1000))
        );
    }

    #[test]
    fn overspending_the_remaining_amount_is_an_error() {
        assert!(apply_step(100, 0, (100, 50, 1), true, true).is_err());
    }
}
