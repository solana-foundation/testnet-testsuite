//! Read-only view of a Manifest market account and simulation-exact book math.
//!
//! The account layout comes from Manifest's published IDL (decoded through the
//! codama SDK types): a 256-byte `MarketFixed` header followed by 80-byte
//! blocks, each a 16-byte red-black-tree node header (`left`, `right`,
//! `parent`, `color`, `payload_type`, padding) and a 64-byte payload
//! (`RestingOrder` or `ClaimedSeat`). Both book sides order their trees so the
//! best price is the maximum, so best-first is a reverse in-order walk.
//!
//! The matching math is written independently to reproduce the program's
//! observable results (no program code is linked or copied; the GPL source
//! is not a dependency). It follows the IDL's account and instruction
//! semantics and is checked against the amounts the deployed program
//! settles. Quantities are in atoms, prices in quote atoms per base atom ×
//! 10^18:
//! - rounding law: `round_up = taker_is_bid != fully_matched_resting_order`
//!   (full fills favor the taker, partial fills favor the maker);
//! - expired orders are skipped;
//! - a global order halts the walk, because swaps built here never pass
//!   global accounts (the program's missing-global-account rule);
//! - Manifest charges no protocol fee.

use borsh::BorshDeserialize;
use manifest_sdk::types::{ClaimedSeat, MarketFixed, OrderType};
use solana_pubkey::Pubkey;

pub const MARKET_FIXED_DISCRIMINANT: u64 = 4_859_840_929_024_028_656;
pub const MARKET_FIXED_SIZE: usize = 256;
pub const MARKET_BLOCK_SIZE: usize = 80;
const NODE_HEADER_SIZE: usize = 16;
const NIL: u32 = u32::MAX;
const D18: u128 = 1_000_000_000_000_000_000;
/// Upper bound for a resting order's base atoms, as enforced by the program.
const ATOM_LIMIT: u128 = u64::MAX as u128;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BookError {
    #[error("account is not a Manifest market: {0}")]
    Malformed(String),
    #[error("arithmetic overflow")]
    Overflow,
}

/// One resting order, best-first within its side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BookOrder {
    pub index: u32,
    /// Quote atoms per base atom × 10^18.
    pub price_raw: u128,
    pub base_atoms: u64,
    pub sequence_number: u64,
    pub trader_index: u32,
    /// 0 never expires.
    pub last_valid_slot: u32,
    pub is_global: bool,
}

impl BookOrder {
    fn is_expired(&self, now_slot: u32) -> bool {
        self.last_valid_slot != 0 && self.last_valid_slot < now_slot
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seat {
    pub index: u32,
    pub trader: Pubkey,
    pub base_withdrawable: u64,
    pub quote_withdrawable: u64,
}

/// Decoded market account.
#[derive(Clone, Debug)]
pub struct Book {
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub base_vault: Pubkey,
    pub quote_vault: Pubkey,
    pub base_decimals: u8,
    pub quote_decimals: u8,
    /// Best (highest) bid first.
    pub bids: Vec<BookOrder>,
    /// Best (lowest) ask first.
    pub asks: Vec<BookOrder>,
    pub seats: Vec<Seat>,
    pub has_free_block: bool,
}

impl Book {
    pub fn parse(data: &[u8]) -> Result<Self, BookError> {
        let header = data
            .get(..MARKET_FIXED_SIZE)
            .ok_or_else(|| BookError::Malformed("shorter than the market header".to_owned()))?;
        let fixed = MarketFixed::try_from_slice(header)
            .map_err(|error| BookError::Malformed(error.to_string()))?;
        if fixed.discriminant != MARKET_FIXED_DISCRIMINANT {
            return Err(BookError::Malformed("wrong discriminant".to_owned()));
        }
        let dynamic = &data[MARKET_FIXED_SIZE..];
        let bids = walk_best_first(dynamic, fixed.bids_root_index)?
            .into_iter()
            .map(|(index, payload)| order(index, payload, true))
            .collect::<Result<Vec<_>, _>>()?;
        let asks = walk_best_first(dynamic, fixed.asks_root_index)?
            .into_iter()
            .map(|(index, payload)| order(index, payload, false))
            .collect::<Result<Vec<_>, _>>()?;
        let seats = walk_best_first(dynamic, fixed.claimed_seats_root_index)?
            .into_iter()
            .map(|(index, payload)| {
                let seat = ClaimedSeat::try_from_slice(payload)
                    .map_err(|error| BookError::Malformed(error.to_string()))?;
                Ok(Seat {
                    index,
                    trader: Pubkey::new_from_array(seat.trader.to_bytes()),
                    base_withdrawable: seat.base_withdrawable_balance,
                    quote_withdrawable: seat.quote_withdrawable_balance,
                })
            })
            .collect::<Result<Vec<_>, BookError>>()?;
        if bids.first().map(|order| order.index) != best_index(fixed.bids_best_index)
            || asks.first().map(|order| order.index) != best_index(fixed.asks_best_index)
        {
            return Err(BookError::Malformed(
                "best-order index does not match the tree maximum".to_owned(),
            ));
        }
        Ok(Self {
            base_mint: Pubkey::new_from_array(fixed.base_mint.to_bytes()),
            quote_mint: Pubkey::new_from_array(fixed.quote_mint.to_bytes()),
            base_vault: Pubkey::new_from_array(fixed.base_vault.to_bytes()),
            quote_vault: Pubkey::new_from_array(fixed.quote_vault.to_bytes()),
            base_decimals: fixed.base_mint_decimals,
            quote_decimals: fixed.quote_mint_decimals,
            bids,
            asks,
            seats,
            has_free_block: fixed.free_list_head_index != NIL,
        })
    }

    pub fn seat(&self, trader: &Pubkey) -> Option<Seat> {
        self.seats
            .iter()
            .copied()
            .find(|seat| seat.trader == *trader)
    }

    /// Resting orders owned by the seat at `trader_index`.
    pub fn orders_of(&self, trader_index: u32) -> impl Iterator<Item = &BookOrder> {
        self.bids
            .iter()
            .chain(&self.asks)
            .filter(move |order| order.trader_index == trader_index)
    }

    /// Fill an immediate-or-cancel taker for `base_atoms` against the opposite
    /// side; returns `(base_traded, quote_traded)`.
    pub fn match_base(
        &self,
        taker_is_bid: bool,
        base_atoms: u64,
        now_slot: u32,
    ) -> Result<(u64, u64), BookError> {
        let side = if taker_is_bid { &self.asks } else { &self.bids };
        let mut remaining = base_atoms;
        let (mut base_traded, mut quote_traded) = (0_u64, 0_u64);
        for resting in side {
            if remaining == 0 {
                break;
            }
            if resting.is_expired(now_slot) {
                continue;
            }
            if resting.is_global {
                break;
            }
            let matched = resting.base_atoms.min(remaining);
            let fully = matched == resting.base_atoms;
            let quote = quote_for_base(resting.price_raw, matched, taker_is_bid != fully)?;
            base_traded = base_traded
                .checked_add(matched)
                .ok_or(BookError::Overflow)?;
            quote_traded = quote_traded.checked_add(quote).ok_or(BookError::Overflow)?;
            remaining -= matched;
        }
        Ok((base_traded, quote_traded))
    }

    /// Quote atoms that matching every available order on the side a taker
    /// hits would trade (bid: cost of all asks; ask: proceeds of all bids),
    /// stopping where matching stops (at a global order).
    pub fn quote_depth(&self, taker_is_bid: bool, now_slot: u32) -> Result<u64, BookError> {
        let side = if taker_is_bid { &self.asks } else { &self.bids };
        let mut depth = 0_u64;
        for resting in side {
            if resting.is_expired(now_slot) {
                continue;
            }
            if resting.is_global {
                break;
            }
            let quote = quote_for_base(resting.price_raw, resting.base_atoms, taker_is_bid)?;
            depth = depth.saturating_add(quote);
        }
        Ok(depth)
    }

    /// The program's base-atom impact for a quote budget: base atoms a taker can
    /// match for `quote_limit` quote atoms (bid: spend; ask: receive).
    pub fn impact_base_atoms(
        &self,
        taker_is_bid: bool,
        quote_limit: u64,
        now_slot: u32,
    ) -> Result<u64, BookError> {
        let side = if taker_is_bid { &self.asks } else { &self.bids };
        let mut total_base = 0_u64;
        let mut remaining_quote = quote_limit;
        for resting in side {
            if resting.is_expired(now_slot) {
                continue;
            }
            let base_limit = base_for_quote(resting.price_raw, remaining_quote, !taker_is_bid)
                .unwrap_or(u64::MAX);
            let matched = resting.base_atoms.min(base_limit);
            let fully = base_limit >= resting.base_atoms;
            let quote = quote_for_base(resting.price_raw, matched, taker_is_bid != fully)?;
            if resting.is_global {
                break;
            }
            total_base = total_base.checked_add(matched).ok_or(BookError::Overflow)?;
            if !fully || quote >= remaining_quote {
                break;
            }
            remaining_quote -= quote;
            if remaining_quote == 0 {
                break;
            }
        }
        Ok(total_base)
    }

    /// Exact-in swap as the program executes it: `(input_traded, output)`.
    pub fn quote_exact_in(
        &self,
        base_in: bool,
        amount_in: u64,
        now_slot: u32,
    ) -> Result<(u64, u64), BookError> {
        if base_in {
            let (base, quote) = self.match_base(false, amount_in, now_slot)?;
            Ok((base, quote))
        } else {
            let base_target = self.impact_base_atoms(true, amount_in, now_slot)?;
            let (base, quote) = self.match_base(true, base_target, now_slot)?;
            Ok((quote, base))
        }
    }

    /// Exact-out swap as the program executes it: `(input_charged, output)`.
    pub fn quote_exact_out(
        &self,
        base_in: bool,
        amount_out: u64,
        now_slot: u32,
    ) -> Result<(u64, u64), BookError> {
        if base_in {
            let base_target = self.impact_base_atoms(false, amount_out, now_slot)?;
            let (base, quote) = self.match_base(false, base_target, now_slot)?;
            Ok((base, quote))
        } else {
            let (base, quote) = self.match_base(true, amount_out, now_slot)?;
            Ok((quote, base))
        }
    }
}

fn best_index(index: u32) -> Option<u32> {
    (index != NIL).then_some(index)
}

fn order(index: u32, payload: &[u8], expect_bid: bool) -> Result<BookOrder, BookError> {
    // RestingOrder's fixed layout, read by hand rather than through the
    // SDK's borsh type: its `order_type` enum would reject a variant byte the
    // fork adds and fail the whole market, where only `Global` matters here.
    // price u128 | num_base_atoms u64 | sequence_number u64 | trader_index u32
    // | last_valid_slot u32 | is_bid bool | order_type u8 | padding [u8; 22]
    let field = |range: std::ops::Range<usize>| {
        payload
            .get(range)
            .ok_or_else(|| BookError::Malformed(format!("order {index} is truncated")))
    };
    let u64_at = |at: usize| -> Result<u64, BookError> {
        Ok(u64::from_le_bytes(field(at..at + 8)?.try_into().map_err(
            |_| BookError::Malformed(format!("order {index} is truncated")),
        )?))
    };
    let u32_at = |at: usize| -> Result<u32, BookError> {
        Ok(u32::from_le_bytes(field(at..at + 4)?.try_into().map_err(
            |_| BookError::Malformed(format!("order {index} is truncated")),
        )?))
    };
    let price_raw = u128::from_le_bytes(
        field(0..16)?
            .try_into()
            .map_err(|_| BookError::Malformed(format!("order {index} is truncated")))?,
    );
    let is_bid = match field(40..41)?[0] {
        0 => false,
        1 => true,
        other => {
            return Err(BookError::Malformed(format!(
                "order {index} has an invalid side byte {other}"
            )));
        }
    };
    if is_bid != expect_bid {
        return Err(BookError::Malformed(format!(
            "order {index} is on the wrong book side"
        )));
    }
    Ok(BookOrder {
        index,
        price_raw,
        base_atoms: u64_at(16)?,
        sequence_number: u64_at(24)?,
        trader_index: u32_at(32)?,
        last_valid_slot: u32_at(36)?,
        is_global: field(41..42)?[0] == OrderType::Global as u8,
    })
}

/// Reverse in-order traversal (maximum first) of the tree rooted at `root`,
/// with bounds, alignment, and cycle guards for untrusted account data.
fn walk_best_first(dynamic: &[u8], root: u32) -> Result<Vec<(u32, &[u8])>, BookError> {
    let max_nodes = dynamic.len() / MARKET_BLOCK_SIZE;
    let node = |index: u32| -> Result<&[u8], BookError> {
        let start = index as usize;
        if !start.is_multiple_of(MARKET_BLOCK_SIZE) {
            return Err(BookError::Malformed(format!("node {index} is misaligned")));
        }
        dynamic
            .get(start..start + MARKET_BLOCK_SIZE)
            .ok_or_else(|| BookError::Malformed(format!("node {index} is out of bounds")))
    };
    let link = |block: &[u8], offset: usize| {
        u32::from_le_bytes([
            block[offset],
            block[offset + 1],
            block[offset + 2],
            block[offset + 3],
        ])
    };
    let mut out = Vec::new();
    let mut stack = Vec::new();
    let mut current = root;
    while current != NIL || !stack.is_empty() {
        while current != NIL {
            if stack.len() + out.len() >= max_nodes {
                return Err(BookError::Malformed("tree contains a cycle".to_owned()));
            }
            let block = node(current)?;
            stack.push(current);
            current = link(block, 4);
        }
        let Some(index) = stack.pop() else { break };
        let block = node(index)?;
        out.push((index, &block[NODE_HEADER_SIZE..]));
        current = link(block, 0);
    }
    Ok(out)
}

fn quote_for_base(price_raw: u128, base_atoms: u64, round_up: bool) -> Result<u64, BookError> {
    let product = price_raw
        .checked_mul(u128::from(base_atoms))
        .ok_or(BookError::Overflow)?;
    let quote = if round_up {
        product.div_ceil(D18)
    } else {
        product / D18
    };
    u64::try_from(quote).map_err(|_| BookError::Overflow)
}

fn base_for_quote(price_raw: u128, quote_atoms: u64, round_up: bool) -> Option<u64> {
    if price_raw == 0 {
        return Some(0);
    }
    let dividend = D18.checked_mul(u128::from(quote_atoms))?;
    let base = if round_up {
        dividend.div_ceil(price_raw)
    } else {
        dividend / price_raw
    };
    (base <= ATOM_LIMIT).then_some(base as u64)
}

/// `(mantissa, exponent)` for the price `numerator / denominator` quote atoms
/// per base atom, choosing the smallest exponent whose mantissa fits `u32`
/// (maximum precision). The result rounds down.
/// `numerator / denominator` as the program's `mantissa × 10^exponent`
/// price, with the most precision a `u32` mantissa holds. `round_up` rounds
/// the last unit up (asks: never quote below the requested price), otherwise
/// down (bids: never above it).
pub fn price_mantissa_exponent(
    numerator: u128,
    denominator: u128,
    round_up: bool,
) -> Result<(u32, i8), BookError> {
    if numerator == 0 || denominator == 0 {
        return Err(BookError::Malformed("price must be positive".to_owned()));
    }
    let divide = |dividend: u128, divisor: u128| {
        if round_up {
            dividend.div_ceil(divisor)
        } else {
            dividend / divisor
        }
    };
    for exponent in -18_i8..=8 {
        let scale = 10_u128.pow(u32::from(exponent.unsigned_abs()));
        let mantissa = if exponent < 0 {
            numerator
                .checked_mul(scale)
                .map(|value| divide(value, denominator))
        } else {
            denominator
                .checked_mul(scale)
                .map(|value| divide(numerator, value))
        };
        let Some(mantissa) = mantissa else { continue };
        if mantissa <= u128::from(u32::MAX) {
            if mantissa == 0 {
                break;
            }
            return Ok((mantissa as u32, exponent));
        }
    }
    Err(BookError::Malformed(
        "price is outside the representable range".to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use manifest_sdk::types::RestingOrder;

    #[test]
    fn order_decoding_tolerates_unknown_order_types() {
        let mut payload = borsh::to_vec(&RestingOrder {
            price: 7 * D18,
            num_base_atoms: 11,
            sequence_number: 13,
            trader_index: 17,
            last_valid_slot: 19,
            is_bid: true,
            order_type: OrderType::Limit,
            padding: [0; 22],
        })
        .expect("serialize");
        let decoded = order(64, &payload, true).expect("limit");
        assert_eq!(
            (
                decoded.price_raw,
                decoded.base_atoms,
                decoded.sequence_number
            ),
            (7 * D18, 11, 13)
        );
        assert_eq!((decoded.trader_index, decoded.last_valid_slot), (17, 19));
        assert!(!decoded.is_global);
        payload[41] = OrderType::Global as u8;
        assert!(order(64, &payload, true).expect("global").is_global);
        payload[41] = 200; // a variant this IDL does not know
        assert!(!order(64, &payload, true).expect("unknown type").is_global);
        assert!(order(64, &payload, false).is_err());
    }
    use solana_address::Address;

    fn order_at(price: u128, base: u64) -> BookOrder {
        BookOrder {
            index: 0,
            price_raw: price,
            base_atoms: base,
            sequence_number: 0,
            trader_index: 0,
            last_valid_slot: 0,
            is_global: false,
        }
    }

    fn book(bids: Vec<BookOrder>, asks: Vec<BookOrder>) -> Book {
        Book {
            base_mint: Pubkey::new_from_array([1; 32]),
            quote_mint: Pubkey::new_from_array([2; 32]),
            base_vault: Pubkey::default(),
            quote_vault: Pubkey::default(),
            base_decimals: 6,
            quote_decimals: 6,
            bids,
            asks,
            seats: vec![],
            has_free_block: false,
        }
    }

    #[test]
    fn sell_base_walks_bids_with_rounding_law() {
        // bids: 100 base @ 2.0, 100 base @ 1.5 (best first). Selling 150:
        // 100 fully matched @2.0 (taker-favoring round up → 200) + 50 partial
        // @1.5 (maker-favoring round down → 75).
        let b = book(
            vec![order_at(2 * D18, 100), order_at(3 * D18 / 2, 100)],
            vec![],
        );
        assert_eq!(b.quote_exact_in(true, 150, 1), Ok((150, 275)));
    }

    #[test]
    fn buy_base_with_quote_mirrors_impact_then_match() {
        // asks: 100 @ 1.5, 100 @ 2.0. 250 quote buys 100 (150 quote) + 50 (100 quote).
        let b = book(
            vec![],
            vec![order_at(3 * D18 / 2, 100), order_at(2 * D18, 100)],
        );
        assert_eq!(b.quote_exact_in(false, 250, 1), Ok((250, 150)));
    }

    #[test]
    fn quote_in_leaves_rounding_dust_and_reports_depth() {
        // 251 quote buys 100 @ 1.5 (150) + 50 @ 2.0 (100); the 51st atom at
        // 2.0 would cost 2, so 1 quote atom is left over.
        let b = book(
            vec![],
            vec![order_at(3 * D18 / 2, 100), order_at(2 * D18, 100)],
        );
        assert_eq!(b.quote_exact_in(false, 251, 1), Ok((250, 150)));
        assert_eq!(b.quote_depth(true, 1), Ok(350));
    }

    #[test]
    fn exact_out_charges_what_the_program_charges() {
        let b = book(
            vec![order_at(2 * D18, 100)],
            vec![order_at(3 * D18 / 2, 100), order_at(2 * D18, 100)],
        );
        // buy exactly 120 base: 100 @1.5 (150) + 20 partial @2.0 (40, round up)
        assert_eq!(b.quote_exact_out(false, 120, 1), Ok((190, 120)));
        // receive exactly 100 quote by selling into the 2.0 bid: 50 base
        assert_eq!(b.quote_exact_out(true, 100, 1), Ok((50, 100)));
    }

    #[test]
    fn expired_skipped_global_halts() {
        let expired = BookOrder {
            last_valid_slot: 5,
            ..order_at(3 * D18, 100)
        };
        let global = BookOrder {
            is_global: true,
            ..order_at(2 * D18, 100)
        };
        let b = book(vec![expired, global, order_at(D18, 100)], vec![]);
        assert_eq!(b.quote_exact_in(true, 50, 10), Ok((0, 0)));
    }

    #[test]
    fn mantissa_exponent_keeps_precision() {
        // 1.0 quote per base with equal decimals.
        assert_eq!(
            price_mantissa_exponent(1, 1, false).expect("fits"),
            (1_000_000_000, -9)
        );
        // 0.000123 → mantissa 1_230_000_000 × 10^-13
        assert_eq!(
            price_mantissa_exponent(123, 1_000_000, false).expect("fits"),
            (1_230_000_000, -13)
        );
        assert_eq!(
            price_mantissa_exponent(5, 2, false).expect("fits"),
            (2_500_000_000, -9)
        );
    }

    #[test]
    fn mantissa_rounds_toward_the_maker() {
        // 1/3 at u32 precision: 3_333_333_333 × 10^-10, last unit up for asks.
        assert_eq!(
            price_mantissa_exponent(1, 3, false),
            Ok((3_333_333_333, -10))
        );
        assert_eq!(
            price_mantissa_exponent(1, 3, true),
            Ok((3_333_333_334, -10))
        );
        // Exact prices are unaffected.
        assert_eq!(price_mantissa_exponent(5, 2, true), Ok((2_500_000_000, -9)));
    }

    fn write_node(dynamic: &mut [u8], index: u32, left: u32, right: u32, payload: &[u8]) {
        let start = index as usize;
        dynamic[start..start + 4].copy_from_slice(&left.to_le_bytes());
        dynamic[start + 4..start + 8].copy_from_slice(&right.to_le_bytes());
        dynamic[start + 8..start + 12].copy_from_slice(&NIL.to_le_bytes());
        dynamic[start + 16..start + 16 + payload.len()].copy_from_slice(payload);
    }

    fn resting(price: u128, base: u64, is_bid: bool) -> Vec<u8> {
        borsh::to_vec(&RestingOrder {
            price,
            num_base_atoms: base,
            sequence_number: 7,
            trader_index: 160,
            last_valid_slot: 0,
            is_bid,
            order_type: OrderType::Limit,
            padding: [0; 22],
        })
        .expect("serializes")
    }

    #[test]
    fn parses_trees_best_first() {
        let mut fixed = MarketFixed {
            discriminant: MARKET_FIXED_DISCRIMINANT,
            version: 1,
            base_mint_decimals: 9,
            quote_mint_decimals: 6,
            base_vault_bump: 0,
            quote_vault_bump: 0,
            padding1: [0; 3],
            base_mint: Address::new_from_array([1; 32]),
            quote_mint: Address::new_from_array([2; 32]),
            base_vault: Address::new_from_array([3; 32]),
            quote_vault: Address::new_from_array([4; 32]),
            order_sequence_number: 0,
            num_bytes_allocated: 0,
            bids_root_index: 0,
            bids_best_index: 80,
            asks_root_index: NIL,
            asks_best_index: NIL,
            claimed_seats_root_index: 160,
            free_list_head_index: NIL,
            padding2: [0],
            quote_volume: 0,
            padding3: [0; 8],
        };
        let mut dynamic = vec![0_u8; 3 * MARKET_BLOCK_SIZE];
        // bids tree: root @0 (price 1), right child @80 (price 2 = best)
        write_node(&mut dynamic, 0, NIL, 80, &resting(D18, 10, true));
        write_node(&mut dynamic, 80, NIL, NIL, &resting(2 * D18, 5, true));
        let seat = borsh::to_vec(&ClaimedSeat {
            trader: Address::new_from_array([9; 32]),
            base_withdrawable_balance: 11,
            quote_withdrawable_balance: 12,
            quote_volume: 0,
            padding: [0; 8],
        })
        .expect("serializes");
        write_node(&mut dynamic, 160, NIL, NIL, &seat);
        let mut data = borsh::to_vec(&fixed).expect("serializes");
        assert_eq!(data.len(), MARKET_FIXED_SIZE);
        data.extend_from_slice(&dynamic);
        let parsed = Book::parse(&data).expect("parses");
        assert_eq!(
            parsed.bids.iter().map(|o| o.price_raw).collect::<Vec<_>>(),
            vec![2 * D18, D18]
        );
        assert_eq!(
            parsed
                .seat(&Pubkey::new_from_array([9; 32]))
                .map(|s| s.index),
            Some(160)
        );
        assert_eq!(parsed.orders_of(160).count(), 2);

        // A self-referencing child is rejected instead of looping.
        write_node(
            &mut data[MARKET_FIXED_SIZE..],
            80,
            NIL,
            0,
            &resting(2 * D18, 5, true),
        );
        assert!(Book::parse(&data).is_err());
        fixed.discriminant = 1;
        let mut wrong = borsh::to_vec(&fixed).expect("serializes");
        wrong.extend_from_slice(&dynamic);
        assert!(Book::parse(&wrong).is_err());
    }
}
