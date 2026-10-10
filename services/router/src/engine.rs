//! Quote engine: direct pairs with per-pair venue splits, 2-hop routes
//! through configured bridge mints, and single-hop ExactOut.
//!
//! Quoting is pure over venue snapshots fetched once per request, so the
//! split search can price many sizes without further RPC.
//!
//! Chaining (docs/router-architecture.md §5) depends on execution:
//! - `direct`: hop 2's instruction amount is fixed at build time, so it is
//!   planned against hop 1's on-chain minimum and the slippage budget is
//!   split across hops; anything received above hop 1's minimum stays in the
//!   intermediate ATA. Bridge `out_amount` is therefore a conservative floor.
//! - `jupiter`: the aggregator forwards each hop's actual output, so hops are
//!   quoted at expected amounts and slippage applies once, to the final output.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use router_client::{Execution, QuoteRequest, QuoteResponse, RouteLeg, SwapMode};
use solana_pubkey::Pubkey;

/// split resolution: how many chunks the input is allocated in
const SPLIT_CHUNKS: u64 = 8;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("invalid address: {0}")]
    BadAddress(String),
    #[error("no route between the given mints")]
    NoRoute,
    #[error("{0}")]
    Unsupported(String),
    #[error("rpc unavailable: {0}")]
    Rpc(String),
}

/// Amounts a venue quote resolves to, in raw atoms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Amounts {
    pub in_amount: u64,
    pub out_amount: u64,
    /// on-chain minimum output (exact-in) or requested output (exact-out)
    pub min_out: u64,
    pub max_in: u64,
    pub fee_amount: u64,
    pub cu_estimate: u32,
}

/// Anything the engine can price: a venue snapshot in production, a
/// constant-product mock in tests.
pub trait QuoteSource: Clone {
    type Quote: Clone;

    fn pool(&self) -> Pubkey;
    fn venue(&self) -> &'static str;
    fn quote(
        &self,
        input: &Pubkey,
        output: &Pubkey,
        amount: u64,
        exact_in: bool,
        slippage_bps: u16,
    ) -> Option<(Self::Quote, Amounts)>;
}

impl QuoteSource for mtm_venues::PoolSnapshot {
    type Quote = mtm_venues::LegQuote;

    fn pool(&self) -> Pubkey {
        mtm_venues::PoolSnapshot::pool(self)
    }

    fn venue(&self) -> &'static str {
        self.kind().as_str()
    }

    fn quote(
        &self,
        input: &Pubkey,
        output: &Pubkey,
        amount: u64,
        exact_in: bool,
        slippage_bps: u16,
    ) -> Option<(Self::Quote, Amounts)> {
        let quote =
            mtm_venues::PoolSnapshot::quote(self, *input, *output, amount, exact_in, slippage_bps)
                .ok()?;
        let amounts = Amounts {
            in_amount: quote.in_amount,
            out_amount: quote.out_amount,
            min_out: quote.min_out,
            max_in: quote.max_in,
            fee_amount: quote.fee_amount,
            cu_estimate: quote.cu_estimate,
        };
        Some((quote, amounts))
    }
}

#[derive(Clone)]
pub struct PlannedLeg<S: QuoteSource> {
    pub source: S,
    pub quote: S::Quote,
    pub amounts: Amounts,
}

/// One sequential step of a route; legs within a segment run in parallel
/// (same-pair splits).
#[derive(Clone)]
pub struct Segment<S: QuoteSource> {
    pub input_mint: Pubkey,
    pub output_mint: Pubkey,
    pub legs: Vec<PlannedLeg<S>>,
}

pub struct PlannedRoute<S: QuoteSource> {
    pub response: QuoteResponse,
    pub segments: Vec<Segment<S>>,
}

/// Parsed request plus the candidate sources for every pair it may touch.
pub struct Request {
    pub input_mint: Pubkey,
    pub output_mint: Pubkey,
    pub amount: u64,
    pub mode: SwapMode,
    pub execution: Execution,
    pub slippage_bps: u16,
    /// Most parallel legs a same-pair split may use. Assembly lowers this
    /// until the route fits one transaction (docs §1: budgets are inputs).
    pub max_split_legs: usize,
    /// Pools the planner must not use (assembly excludes the widest leg
    /// when a route does not fit one transaction).
    pub excluded: HashSet<Pubkey>,
    /// CU charged for the aggregator itself on `jupiter` routes.
    pub jupiter_overhead_cu: u32,
}

/// Split width tried first; three venues trade every pair at most.
pub const DEFAULT_MAX_SPLIT_LEGS: usize = 3;

impl Request {
    pub fn parse(req: &QuoteRequest) -> Result<Self, EngineError> {
        let input_mint = Pubkey::from_str(&req.input_mint)
            .map_err(|_| EngineError::BadAddress(req.input_mint.clone()))?;
        let output_mint = Pubkey::from_str(&req.output_mint)
            .map_err(|_| EngineError::BadAddress(req.output_mint.clone()))?;
        if input_mint == output_mint {
            return Err(EngineError::NoRoute);
        }
        Ok(Self {
            input_mint,
            output_mint,
            amount: req.amount,
            mode: req.mode,
            execution: req.execution,
            slippage_bps: req.slippage_bps,
            max_split_legs: DEFAULT_MAX_SPLIT_LEGS,
            excluded: HashSet::new(),
            jupiter_overhead_cu: 0,
        })
    }

    /// Every unordered pair the planner may quote, given the bridge mints.
    pub fn pairs(&self, bridges: &[Pubkey]) -> Vec<(Pubkey, Pubkey)> {
        let mut pairs = vec![(self.input_mint, self.output_mint)];
        if self.mode == SwapMode::ExactIn {
            for bridge in bridges {
                if *bridge != self.input_mint && *bridge != self.output_mint {
                    pairs.push((self.input_mint, *bridge));
                    pairs.push((*bridge, self.output_mint));
                }
            }
        }
        pairs
    }
}

/// Sources grouped by unordered pair.
pub struct Book<S> {
    by_pair: HashMap<(Pubkey, Pubkey), Vec<S>>,
}

impl<S: QuoteSource> Book<S> {
    pub fn new(sources: Vec<((Pubkey, Pubkey), S)>) -> Self {
        let mut by_pair: HashMap<(Pubkey, Pubkey), Vec<S>> = HashMap::new();
        for ((a, b), source) in sources {
            by_pair.entry(pair_key(a, b)).or_default().push(source);
        }
        Self { by_pair }
    }

    fn sources(&self, a: &Pubkey, b: &Pubkey, excluded: &HashSet<Pubkey>) -> Vec<S> {
        self.by_pair
            .get(&pair_key(*a, *b))
            .map(|sources| {
                sources
                    .iter()
                    .filter(|source| !excluded.contains(&source.pool()))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn pair_key(a: Pubkey, b: Pubkey) -> (Pubkey, Pubkey) {
    if a <= b { (a, b) } else { (b, a) }
}

pub fn plan<S: QuoteSource>(
    request: &Request,
    book: &Book<S>,
    bridges: &[Pubkey],
    state_slot: u64,
) -> Result<PlannedRoute<S>, EngineError> {
    let segments = match request.mode {
        SwapMode::ExactOut => {
            if request.execution == Execution::Jupiter {
                // PLACEHOLDER: the fork's exact_out_route_v2 is not wired yet.
                return Err(EngineError::Unsupported(
                    "exact_out through the Jupiter fork is not implemented".to_owned(),
                ));
            }
            plan_exact_out(request, book)?
        }
        SwapMode::ExactIn => plan_exact_in(request, book, bridges)?,
    };
    let response = build_response(request, &segments, state_slot);
    Ok(PlannedRoute { response, segments })
}

fn plan_exact_out<S: QuoteSource>(
    request: &Request,
    book: &Book<S>,
) -> Result<Vec<Segment<S>>, EngineError> {
    // ExactOut stays single-hop, best single venue
    let leg = book
        .sources(&request.input_mint, &request.output_mint, &request.excluded)
        .iter()
        .filter_map(|source| {
            source
                .quote(
                    &request.input_mint,
                    &request.output_mint,
                    request.amount,
                    false,
                    request.slippage_bps,
                )
                .map(|(quote, amounts)| PlannedLeg {
                    source: source.clone(),
                    quote,
                    amounts,
                })
        })
        .min_by_key(|leg| leg.amounts.in_amount)
        .ok_or(EngineError::NoRoute)?;
    Ok(vec![Segment {
        input_mint: request.input_mint,
        output_mint: request.output_mint,
        legs: vec![leg],
    }])
}

fn plan_exact_in<S: QuoteSource>(
    request: &Request,
    book: &Book<S>,
    bridges: &[Pubkey],
) -> Result<Vec<Segment<S>>, EngineError> {
    let (input, output) = (&request.input_mint, &request.output_mint);
    // candidate 1: direct, with per-pair splits
    let direct = plan_split(
        &book.sources(input, output, &request.excluded),
        input,
        output,
        request.amount,
        request.slippage_bps,
        request.max_split_legs,
    )
    .map(|legs| {
        vec![Segment {
            input_mint: *input,
            output_mint: *output,
            legs,
        }]
    });

    // candidates 2..n: 2-hop through each configured bridge mint
    let mut best = direct;
    for bridge in bridges {
        if bridge == input || bridge == output {
            continue;
        }
        let Some(candidate) = plan_bridge(
            &book.sources(input, bridge, &request.excluded),
            &book.sources(bridge, output, &request.excluded),
            input,
            bridge,
            output,
            request.amount,
            request.slippage_bps,
            request.execution,
        ) else {
            continue;
        };
        if best
            .as_ref()
            .is_none_or(|current| route_out(&candidate) > route_out(current))
        {
            best = Some(candidate);
        }
    }
    best.ok_or(EngineError::NoRoute)
}

/// Plan one A→bridge→B candidate: best single venue per hop.
#[allow(clippy::too_many_arguments)]
fn plan_bridge<S: QuoteSource>(
    hop1: &[S],
    hop2: &[S],
    input: &Pubkey,
    bridge: &Pubkey,
    output: &Pubkey,
    amount: u64,
    slippage_bps: u16,
    execution: Execution,
) -> Option<Vec<Segment<S>>> {
    if hop1.is_empty() || hop2.is_empty() {
        return None;
    }
    let (hop_slippage, final_slippage) = match execution {
        Execution::Direct => (slippage_bps / 2, slippage_bps / 2),
        Execution::Jupiter => (0, slippage_bps),
    };
    let first = best_single(hop1, input, bridge, amount, hop_slippage)?;
    let hop2_amount = match execution {
        // hop 2 spends exactly what hop 1's instruction guarantees
        Execution::Direct => first.amounts.min_out,
        Execution::Jupiter => first.amounts.out_amount,
    };
    if hop2_amount == 0 {
        return None;
    }
    let second = best_single(hop2, bridge, output, hop2_amount, final_slippage)?;
    Some(vec![
        Segment {
            input_mint: *input,
            output_mint: *bridge,
            legs: vec![first],
        },
        Segment {
            input_mint: *bridge,
            output_mint: *output,
            legs: vec![second],
        },
    ])
}

/// total out of a route = out of its final segment
fn route_out<S: QuoteSource>(segments: &[Segment<S>]) -> u64 {
    segments
        .last()
        .map(|segment| segment.legs.iter().map(|leg| leg.amounts.out_amount).sum())
        .unwrap_or(0)
}

/// Compute units the route is expected to consume: every leg's estimate,
/// plus the aggregator's own overhead for `jupiter` execution.
pub fn route_cu_estimate<S: QuoteSource>(
    segments: &[Segment<S>],
    execution: Execution,
    jupiter_overhead_cu: u32,
) -> u32 {
    let legs = segments
        .iter()
        .flat_map(|segment| &segment.legs)
        .fold(0_u32, |total, leg| {
            total.saturating_add(leg.amounts.cu_estimate)
        });
    match execution {
        Execution::Direct => legs,
        Execution::Jupiter => legs.saturating_add(jupiter_overhead_cu),
    }
}

/// Floor enforced on the route's final output.
pub fn route_min_out<S: QuoteSource>(
    segments: &[Segment<S>],
    execution: Execution,
    slippage_bps: u16,
) -> u64 {
    match execution {
        Execution::Direct => segments
            .last()
            .map(|segment| segment.legs.iter().map(|leg| leg.amounts.min_out).sum())
            .unwrap_or(0),
        Execution::Jupiter => apply_bps_floor(route_out(segments), slippage_bps),
    }
}

fn apply_bps_floor(amount: u64, bps: u16) -> u64 {
    (u128::from(amount) * u128::from(10_000 - bps.min(10_000)) / 10_000) as u64
}

fn build_response<S: QuoteSource>(
    request: &Request,
    segments: &[Segment<S>],
    state_slot: u64,
) -> QuoteResponse {
    let in_amount = segments
        .first()
        .map(|segment| segment.legs.iter().map(|leg| leg.amounts.in_amount).sum())
        .unwrap_or(0);
    let route = segments
        .iter()
        .flat_map(|segment| {
            segment.legs.iter().map(|leg| RouteLeg {
                venue: leg.source.venue().to_string(),
                pool: leg.source.pool().to_string(),
                input_mint: segment.input_mint.to_string(),
                output_mint: segment.output_mint.to_string(),
                in_amount: leg.amounts.in_amount,
                out_amount: leg.amounts.out_amount,
                fee_amount: leg.amounts.fee_amount,
                price_impact_bps: 0,
                cu_estimate: leg.amounts.cu_estimate,
            })
        })
        .collect();
    QuoteResponse {
        execution: request.execution,
        in_amount,
        out_amount: route_out(segments),
        min_out_amount: route_min_out(segments, request.execution, request.slippage_bps),
        price_impact_bps: 0,
        route,
        state_slot,
        cu_estimate: route_cu_estimate(segments, request.execution, request.jupiter_overhead_cu),
        compute_unit_limit: None,
        tx_base64: None,
    }
}

/// Greedy chunk allocation by marginal output across same-pair venues
/// (out(in) is concave per venue, so equalizing marginals is optimal),
/// using at most `max_legs` venues.
fn plan_split<S: QuoteSource>(
    sources: &[S],
    input: &Pubkey,
    output: &Pubkey,
    amount: u64,
    slippage_bps: u16,
    max_legs: usize,
) -> Option<Vec<PlannedLeg<S>>> {
    if sources.is_empty() {
        return None;
    }
    let legs = allocate(sources, input, output, amount, slippage_bps)?;
    if legs.len() <= max_legs.max(1) {
        return Some(legs);
    }
    // Too wide: keep the venues that took the most input and re-allocate.
    let mut ranked = legs;
    ranked.sort_by_key(|leg| std::cmp::Reverse(leg.amounts.in_amount));
    let kept: Vec<S> = ranked
        .into_iter()
        .take(max_legs.max(1))
        .map(|leg| leg.source)
        .collect();
    allocate(&kept, input, output, amount, slippage_bps)
}

fn allocate<S: QuoteSource>(
    sources: &[S],
    input: &Pubkey,
    output: &Pubkey,
    amount: u64,
    slippage_bps: u16,
) -> Option<Vec<PlannedLeg<S>>> {
    let single = best_single(sources, input, output, amount, slippage_bps);
    if sources.len() == 1 || amount < SPLIT_CHUNKS * 4 {
        return single.map(|leg| vec![leg]);
    }
    let quote_out = |source: &S, amount_in: u64| -> Option<u64> {
        if amount_in == 0 {
            return Some(0);
        }
        source
            .quote(input, output, amount_in, true, slippage_bps)
            .map(|(_, amounts)| amounts.out_amount)
    };
    let chunk = amount / SPLIT_CHUNKS;
    let mut allocation = vec![0_u64; sources.len()];
    let mut current_out = vec![0_u64; sources.len()];
    let mut remaining = amount;
    while remaining > 0 {
        let this_chunk = if remaining < 2 * chunk {
            remaining
        } else {
            chunk
        };
        let best = sources
            .iter()
            .enumerate()
            .filter_map(|(i, source)| {
                let new_out = quote_out(source, allocation[i] + this_chunk)?;
                new_out
                    .checked_sub(current_out[i])
                    .map(|marginal| (i, marginal, new_out))
            })
            .max_by_key(|&(_, marginal, _)| marginal);
        let Some((i, marginal, new_out)) = best else {
            break;
        };
        if marginal == 0 {
            break;
        }
        allocation[i] += this_chunk;
        current_out[i] = new_out;
        remaining -= this_chunk;
    }
    let active = allocation.iter().filter(|&&a| a > 0).count();
    if active <= 1 || remaining > 0 {
        return single.map(|leg| vec![leg]);
    }
    let legs: Vec<PlannedLeg<S>> = sources
        .iter()
        .zip(&allocation)
        .filter(|(_, allocated)| **allocated > 0)
        .map(|(source, allocated)| {
            source
                .quote(input, output, *allocated, true, slippage_bps)
                .map(|(quote, amounts)| PlannedLeg {
                    source: source.clone(),
                    quote,
                    amounts,
                })
        })
        .collect::<Option<_>>()?;
    let split_out: u64 = legs.iter().map(|leg| leg.amounts.out_amount).sum();
    match single {
        Some(single) if single.amounts.out_amount >= split_out => Some(vec![single]),
        _ => Some(legs),
    }
}

fn best_single<S: QuoteSource>(
    sources: &[S],
    input: &Pubkey,
    output: &Pubkey,
    amount: u64,
    slippage_bps: u16,
) -> Option<PlannedLeg<S>> {
    sources
        .iter()
        .filter_map(|source| {
            source
                .quote(input, output, amount, true, slippage_bps)
                .map(|(quote, amounts)| PlannedLeg {
                    source: source.clone(),
                    quote,
                    amounts,
                })
        })
        .max_by_key(|leg| leg.amounts.out_amount)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Constant-product pool with a 0.25% fee, enough to exercise the
    /// planner's allocation and chaining logic.
    #[derive(Clone)]
    struct Cpmm {
        pool: Pubkey,
        mint_a: Pubkey,
        mint_b: Pubkey,
        reserve_a: u64,
        reserve_b: u64,
    }

    impl QuoteSource for Cpmm {
        type Quote = ();

        fn pool(&self) -> Pubkey {
            self.pool
        }

        fn venue(&self) -> &'static str {
            "test-cpmm"
        }

        fn quote(
            &self,
            input: &Pubkey,
            output: &Pubkey,
            amount: u64,
            exact_in: bool,
            slippage_bps: u16,
        ) -> Option<((), Amounts)> {
            let (r_in, r_out) = if *input == self.mint_a && *output == self.mint_b {
                (self.reserve_a, self.reserve_b)
            } else if *input == self.mint_b && *output == self.mint_a {
                (self.reserve_b, self.reserve_a)
            } else {
                return None;
            };
            let (r_in, r_out) = (u128::from(r_in), u128::from(r_out));
            if exact_in {
                let net = u128::from(amount) * 9_975 / 10_000;
                let out = (r_out * net / (r_in + net)) as u64;
                (out > 0).then(|| {
                    (
                        (),
                        Amounts {
                            in_amount: amount,
                            out_amount: out,
                            min_out: apply_bps_floor(out, slippage_bps),
                            max_in: amount,
                            fee_amount: amount - net as u64,
                            cu_estimate: 1,
                        },
                    )
                })
            } else {
                let out = u128::from(amount);
                (out < r_out).then(|| {
                    let net = (r_in * out).div_ceil(r_out - out);
                    let gross = (net * 10_000).div_ceil(9_975) as u64;
                    (
                        (),
                        Amounts {
                            in_amount: gross,
                            out_amount: amount,
                            min_out: amount,
                            max_in: gross,
                            fee_amount: 0,
                            cu_estimate: 1,
                        },
                    )
                })
            }
        }
    }

    fn key(n: u8) -> Pubkey {
        Pubkey::new_from_array([n; 32])
    }

    fn pool(n: u8, a: u8, b: u8, ra: u64, rb: u64) -> ((Pubkey, Pubkey), Cpmm) {
        (
            (key(a), key(b)),
            Cpmm {
                pool: key(n),
                mint_a: key(a),
                mint_b: key(b),
                reserve_a: ra,
                reserve_b: rb,
            },
        )
    }

    fn request(amount: u64, execution: Execution) -> Request {
        Request {
            input_mint: key(4),
            output_mint: key(5),
            amount,
            mode: SwapMode::ExactIn,
            execution,
            slippage_bps: 50,
            max_split_legs: DEFAULT_MAX_SPLIT_LEGS,
            excluded: HashSet::new(),
            jupiter_overhead_cu: 0,
        }
    }

    #[test]
    fn split_beats_single_across_uneven_pools() {
        let book = Book::new(vec![
            pool(1, 4, 5, 1_000_000_000, 1_000_000_000),
            pool(2, 4, 5, 250_000_000, 250_000_000),
        ]);
        let route = plan(&request(200_000_000, Execution::Direct), &book, &[], 1).expect("plans");
        let legs = &route.segments[0].legs;
        assert_eq!(legs.len(), 2, "expected a split");
        let in_sum: u64 = legs.iter().map(|leg| leg.amounts.in_amount).sum();
        assert_eq!(in_sum, 200_000_000);
        let single = best_single(
            &book.sources(&key(4), &key(5), &HashSet::new()),
            &key(4),
            &key(5),
            200_000_000,
            50,
        )
        .expect("single");
        assert!(route.response.out_amount > single.amounts.out_amount);
        assert_eq!(
            route.response.min_out_amount,
            legs.iter().map(|leg| leg.amounts.min_out).sum::<u64>()
        );
    }

    #[test]
    fn split_width_is_capped() {
        let book = Book::new(vec![
            pool(1, 4, 5, 1_000_000_000, 1_000_000_000),
            pool(2, 4, 5, 600_000_000, 600_000_000),
            pool(3, 4, 5, 250_000_000, 250_000_000),
        ]);
        let wide = plan(&request(400_000_000, Execution::Direct), &book, &[], 1).expect("plans");
        assert_eq!(wide.segments[0].legs.len(), 3);
        let capped = Request {
            max_split_legs: 2,
            ..request(400_000_000, Execution::Direct)
        };
        let narrow = plan(&capped, &book, &[], 1).expect("plans");
        let legs = &narrow.segments[0].legs;
        assert_eq!(legs.len(), 2);
        assert!(legs.iter().all(|leg| leg.source.pool != key(3)));
        assert_eq!(
            legs.iter().map(|l| l.amounts.in_amount).sum::<u64>(),
            400_000_000
        );
        let single = Request {
            max_split_legs: 1,
            ..capped
        };
        assert_eq!(
            plan(&single, &book, &[], 1).expect("plans").segments[0]
                .legs
                .len(),
            1
        );
    }

    #[test]
    fn cu_estimate_sums_legs_and_jupiter_overhead() {
        let book = Book::new(vec![
            pool(1, 4, 6, 1_000_000_000, 1_000_000_000),
            pool(2, 6, 5, 1_000_000_000, 1_000_000_000),
        ]);
        let direct =
            plan(&request(10_000_000, Execution::Direct), &book, &[key(6)], 1).expect("plans");
        assert_eq!(direct.response.cu_estimate, 2);
        assert!(direct.response.route.iter().all(|leg| leg.cu_estimate == 1));
        let jupiter = Request {
            jupiter_overhead_cu: 60_000,
            ..request(10_000_000, Execution::Jupiter)
        };
        let routed = plan(&jupiter, &book, &[key(6)], 1).expect("plans");
        assert_eq!(routed.response.cu_estimate, 60_002);
        assert_eq!(routed.response.compute_unit_limit, None);
    }

    #[test]
    fn tiny_amounts_stay_single() {
        let book = Book::new(vec![
            pool(1, 4, 5, 1_000_000_000, 1_000_000_000),
            pool(2, 4, 5, 250_000_000, 250_000_000),
        ]);
        let route = plan(&request(16, Execution::Direct), &book, &[], 1).expect("plans");
        assert_eq!(route.segments[0].legs.len(), 1);
    }

    #[test]
    fn direct_bridge_chains_conservatively() {
        let book = Book::new(vec![
            pool(1, 4, 6, 1_000_000_000, 1_000_000_000),
            pool(2, 6, 5, 1_000_000_000, 1_000_000_000),
        ]);
        let route =
            plan(&request(10_000_000, Execution::Direct), &book, &[key(6)], 1).expect("plans");
        assert_eq!(route.segments.len(), 2);
        let (hop1, hop2) = (&route.segments[0].legs[0], &route.segments[1].legs[0]);
        // slippage budget split across hops; hop 2 spends hop 1's guarantee
        assert_eq!(
            hop1.amounts.min_out,
            apply_bps_floor(hop1.amounts.out_amount, 25)
        );
        assert_eq!(hop2.amounts.in_amount, hop1.amounts.min_out);
        assert_eq!(
            hop2.amounts.min_out,
            apply_bps_floor(hop2.amounts.out_amount, 25)
        );
        assert_eq!(route.segments[0].output_mint, route.segments[1].input_mint);
    }

    #[test]
    fn jupiter_bridge_forwards_expected_amounts() {
        let book = Book::new(vec![
            pool(1, 4, 6, 1_000_000_000, 1_000_000_000),
            pool(2, 6, 5, 1_000_000_000, 1_000_000_000),
        ]);
        let direct =
            plan(&request(10_000_000, Execution::Direct), &book, &[key(6)], 1).expect("plans");
        let jupiter = plan(
            &request(10_000_000, Execution::Jupiter),
            &book,
            &[key(6)],
            1,
        )
        .expect("plans");
        let (hop1, hop2) = (&jupiter.segments[0].legs[0], &jupiter.segments[1].legs[0]);
        assert_eq!(hop2.amounts.in_amount, hop1.amounts.out_amount);
        assert!(jupiter.response.out_amount > direct.response.out_amount);
        assert_eq!(
            jupiter.response.min_out_amount,
            apply_bps_floor(jupiter.response.out_amount, 50)
        );
    }

    #[test]
    fn bridge_needs_both_hops() {
        let book = Book::new(vec![pool(1, 4, 6, 1_000_000_000, 1_000_000_000)]);
        assert!(matches!(
            plan(&request(1_000_000, Execution::Direct), &book, &[key(6)], 1),
            Err(EngineError::NoRoute)
        ));
    }

    #[test]
    fn exact_out_picks_cheapest_input() {
        let book = Book::new(vec![
            pool(1, 4, 5, 1_000_000_000, 1_000_000_000),
            pool(2, 4, 5, 500_000_000, 1_000_000_000),
        ]);
        let req = Request {
            mode: SwapMode::ExactOut,
            ..request(1_000_000, Execution::Direct)
        };
        let route = plan(&req, &book, &[], 1).expect("plans");
        assert_eq!(route.segments[0].legs[0].source.pool, key(2));
        assert_eq!(route.response.out_amount, 1_000_000);
        let excluded = Request {
            excluded: HashSet::from([key(2)]),
            ..req
        };
        let route = plan(&excluded, &book, &[], 1).expect("plans");
        assert_eq!(route.segments[0].legs[0].source.pool, key(1));
        let jupiter = Request {
            execution: Execution::Jupiter,
            ..excluded
        };
        assert!(matches!(
            plan(&jupiter, &book, &[], 1),
            Err(EngineError::Unsupported(_))
        ));
    }
}
