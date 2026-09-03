//! The pricing engine: evaluates the instrument registry against underlying
//! ticks and its own clock, producing PricePoints for the API.
//!
//! Float policy: generators (gbm/noise/beta) compute in f64 and quantize
//! immediately via `Price::from_f64_lossy`; everything else stays in
//! Decimal/fixed-point. All randomness is seeded per instrument, so a config
//! file plus an underlying tick tape defines a reproducible price world.

pub mod spec;

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use anyhow::{Context, bail};
use mtm_common::Symbol;
use oracle_client::{Price, PriceData, PricePoint, PriceSource};
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use rand_distr::{Distribution, StandardNormal};
use rust_decimal::Decimal;
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use spec::{BaseSourceConfig, InstrumentConfig, TransformConfig};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::state::AppState;

const ENGINE_TICK_MS: u64 = 250;

#[derive(Debug, Clone)]
struct Snapshot {
    value: Decimal,
    conf_ratio: Option<Decimal>,
    publish_time_us: Option<i64>,
    slot: Option<u64>,
    source: PriceSource,
}

#[derive(Debug, Clone)]
struct EvalCtx {
    value: Decimal,
    conf_ratio: Option<Decimal>,
    publish_time_us: Option<i64>,
    slot: Option<u64>,
    source: PriceSource,
    derived: bool,
}

enum BaseState {
    Underlying {
        feed: Symbol,
    },
    Cross {
        base: Symbol,
        quote: Symbol,
    },
    Basket {
        legs: Vec<(Symbol, Decimal)>,
        rebase: Option<Decimal>,
        factor: Option<Decimal>,
    },
    Peg {
        target: Decimal,
    },
    Gbm {
        price: f64,
        sigma_daily: f64,
        drift_daily: f64,
        tick_ms: u64,
        rng: Box<ChaCha8Rng>,
    },
}

struct LagSample {
    at_us: i64,
    ctx: EvalCtx,
}

enum TransformState {
    Scale {
        factor: Decimal,
        offset: Decimal,
    },
    Invert,
    Beta {
        beta: f64,
        anchor: f64,
        initial: f64,
    },
    Lag {
        us: i64,
        buf: VecDeque<LagSample>,
    },
    Noise {
        sigma: f64,
        halflife_us: f64,
        x: f64,
        last_us: Option<i64>,
        rng: Box<ChaCha8Rng>,
    },
}

struct InstrumentRuntime {
    symbol: Symbol,
    expo: i32,
    base: BaseState,
    transforms: Vec<TransformState>,
    next_due_us: i64,
}

pub struct Engine {
    instruments: Vec<InstrumentRuntime>,
    by_feed: HashMap<Symbol, Vec<usize>>,
    timed: Vec<usize>,
    latest: HashMap<Symbol, Snapshot>,
}

fn parse_decimal(s: &str, what: &str) -> anyhow::Result<Decimal> {
    s.parse::<Decimal>()
        .with_context(|| format!("invalid decimal for {what}: {s:?}"))
}

impl Engine {
    pub fn from_config(
        instruments: &[InstrumentConfig],
        underlyings: &[Symbol],
    ) -> anyhow::Result<Self> {
        let known: std::collections::HashSet<&Symbol> = underlyings.iter().collect();
        let check_feed = |feed: &Symbol| -> anyhow::Result<()> {
            if !known.contains(feed) {
                bail!("instrument references unknown underlying feed {feed}");
            }
            Ok(())
        };

        let mut runtimes = Vec::with_capacity(instruments.len());
        let mut by_feed: HashMap<Symbol, Vec<usize>> = HashMap::new();
        let mut timed = Vec::new();
        let mut seen = std::collections::HashSet::new();

        for (idx, cfg) in instruments.iter().enumerate() {
            let symbol = cfg
                .resolved_symbol()
                .with_context(|| format!("instrument #{idx}: needs `symbol` or `mint`"))?;
            if !seen.insert(symbol.clone()) {
                bail!("duplicate instrument symbol {symbol}");
            }

            let mut feeds_used: Vec<Symbol> = Vec::new();
            let base = match &cfg.base {
                BaseSourceConfig::Underlying { feed } => {
                    check_feed(feed)?;
                    feeds_used.push(feed.clone());
                    BaseState::Underlying { feed: feed.clone() }
                }
                BaseSourceConfig::Cross { base, quote } => {
                    check_feed(base)?;
                    check_feed(quote)?;
                    feeds_used.push(base.clone());
                    feeds_used.push(quote.clone());
                    BaseState::Cross {
                        base: base.clone(),
                        quote: quote.clone(),
                    }
                }
                BaseSourceConfig::Basket { legs, rebase } => {
                    let mut parsed = Vec::with_capacity(legs.len());
                    for leg in legs {
                        check_feed(&leg.feed)?;
                        feeds_used.push(leg.feed.clone());
                        parsed.push((leg.feed.clone(), parse_decimal(&leg.weight, "weight")?));
                    }
                    BaseState::Basket {
                        legs: parsed,
                        rebase: rebase
                            .as_deref()
                            .map(|r| parse_decimal(r, "rebase"))
                            .transpose()?,
                        factor: None,
                    }
                }
                BaseSourceConfig::Peg { target } => BaseState::Peg {
                    target: parse_decimal(target, "peg target")?,
                },
                BaseSourceConfig::Gbm {
                    initial,
                    daily_vol_bps,
                    daily_drift_bps,
                    seed,
                    tick_ms,
                } => BaseState::Gbm {
                    price: parse_decimal(initial, "gbm initial")?
                        .to_f64()
                        .context("gbm initial out of f64 range")?,
                    sigma_daily: f64::from(*daily_vol_bps) / 10_000.0,
                    drift_daily: f64::from(*daily_drift_bps) / 10_000.0,
                    tick_ms: (*tick_ms).max(ENGINE_TICK_MS),
                    rng: Box::new(ChaCha8Rng::seed_from_u64(*seed)),
                },
            };

            let transforms = cfg
                .transforms
                .iter()
                .map(|t| -> anyhow::Result<TransformState> {
                    Ok(match t {
                        TransformConfig::Scale { factor, offset } => TransformState::Scale {
                            factor: parse_decimal(factor, "scale factor")?,
                            offset: offset
                                .as_deref()
                                .map(|o| parse_decimal(o, "scale offset"))
                                .transpose()?
                                .unwrap_or(Decimal::ZERO),
                        },
                        TransformConfig::Invert => TransformState::Invert,
                        TransformConfig::Beta {
                            beta,
                            anchor,
                            initial,
                        } => TransformState::Beta {
                            beta: *beta,
                            anchor: parse_decimal(anchor, "beta anchor")?
                                .to_f64()
                                .context("beta anchor out of f64 range")?,
                            initial: parse_decimal(initial, "beta initial")?
                                .to_f64()
                                .context("beta initial out of f64 range")?,
                        },
                        TransformConfig::Lag { ms } => TransformState::Lag {
                            us: i64::try_from(*ms)
                                .ok()
                                .and_then(|ms| ms.checked_mul(1000))
                                .context("lag too large")?,
                            buf: VecDeque::new(),
                        },
                        TransformConfig::Noise {
                            sigma_bps,
                            halflife_s,
                            seed,
                        } => TransformState::Noise {
                            sigma: f64::from(*sigma_bps) / 10_000.0,
                            halflife_us: (*halflife_s as f64) * 1_000_000.0,
                            x: 0.0,
                            last_us: None,
                            rng: Box::new(ChaCha8Rng::seed_from_u64(*seed)),
                        },
                    })
                })
                .collect::<anyhow::Result<Vec<_>>>()?;

            let runtime_idx = runtimes.len();
            let clock_driven = matches!(base, BaseState::Gbm { .. } | BaseState::Peg { .. });
            if clock_driven {
                timed.push(runtime_idx);
            }
            for feed in feeds_used {
                by_feed.entry(feed).or_default().push(runtime_idx);
            }
            runtimes.push(InstrumentRuntime {
                symbol,
                expo: cfg.expo,
                base,
                transforms,
                next_due_us: 0,
            });
        }

        info!(
            instruments = runtimes.len(),
            clock_driven = timed.len(),
            "pricing engine built"
        );
        Ok(Self {
            instruments: runtimes,
            by_feed,
            timed,
            latest: HashMap::new(),
        })
    }

    pub fn on_underlying(&mut self, point: &PricePoint, now_us: i64) -> Vec<PricePoint> {
        let reduced = match &point.data {
            PriceData::Aggregate { price, conf, .. } => price.to_decimal().map(|value| {
                let conf_ratio = conf
                    .and_then(|c| c.to_decimal())
                    .and_then(|c| c.checked_div(value));
                (value, conf_ratio)
            }),
            PriceData::TopOfBook { bid, ask, .. } => {
                bid.to_decimal().zip(ask.to_decimal()).and_then(|(b, a)| {
                    let two = Decimal::TWO;
                    let mid = b.checked_add(a)?.checked_div(two)?;
                    let half_spread = a.checked_sub(b)?.abs().checked_div(two)?;
                    Some((mid, half_spread.checked_div(mid)))
                })
            }
            PriceData::Trade { price, .. } => price.to_decimal().map(|value| (value, None)),
            _ => return Vec::new(), // future observation kinds
        };
        let Some((value, conf_ratio)) = reduced else {
            warn!(symbol = %point.symbol, "underlying price out of Decimal range");
            return Vec::new();
        };
        self.latest.insert(
            point.symbol.clone(),
            Snapshot {
                value,
                conf_ratio,
                publish_time_us: point.publish_time_us,
                slot: point.slot,
                source: point.source,
            },
        );

        let Some(indices) = self.by_feed.get(&point.symbol).cloned() else {
            return Vec::new();
        };
        indices
            .into_iter()
            .filter_map(|idx| eval(&mut self.instruments[idx], &self.latest, now_us))
            .collect()
    }

    pub fn on_tick(&mut self, now_us: i64) -> Vec<PricePoint> {
        let mut out = Vec::new();
        for &idx in &self.timed.clone() {
            let inst = &mut self.instruments[idx];
            if now_us < inst.next_due_us {
                continue;
            }
            let interval_us = match &inst.base {
                BaseState::Gbm { tick_ms, .. } => (*tick_ms as i64) * 1000,
                _ => (ENGINE_TICK_MS as i64) * 1000 * 4, // pegs re-publish at 1s
            };
            inst.next_due_us = now_us + interval_us;
            if let Some(point) = eval(inst, &self.latest, now_us) {
                out.push(point);
            }
        }
        out
    }
}

fn eval(
    inst: &mut InstrumentRuntime,
    latest: &HashMap<Symbol, Snapshot>,
    now_us: i64,
) -> Option<PricePoint> {
    let mut ctx = eval_base(&mut inst.base, latest, now_us)?;
    for transform in &mut inst.transforms {
        ctx = apply_transform(transform, ctx, now_us)?;
    }

    let price = Price::from_decimal(ctx.value, inst.expo).ok()?;
    let conf = ctx
        .conf_ratio
        .and_then(|r| ctx.value.checked_mul(r))
        .and_then(|c| Price::from_decimal(c, inst.expo).ok());
    Some(PricePoint {
        symbol: inst.symbol.clone(),
        data: PriceData::Aggregate {
            price,
            conf,
            ema: None,
        },
        publish_time_us: ctx.publish_time_us,
        received_at_us: now_us,
        slot: ctx.slot,
        source: if ctx.derived {
            PriceSource::Derived
        } else {
            ctx.source
        },
    })
}

fn eval_base(
    base: &mut BaseState,
    latest: &HashMap<Symbol, Snapshot>,
    now_us: i64,
) -> Option<EvalCtx> {
    match base {
        BaseState::Underlying { feed } => {
            let snap = latest.get(feed)?;
            Some(EvalCtx {
                value: snap.value,
                conf_ratio: snap.conf_ratio,
                publish_time_us: snap.publish_time_us,
                slot: snap.slot,
                source: snap.source,
                derived: false,
            })
        }
        BaseState::Cross { base, quote } => {
            let b = latest.get(base)?;
            let q = latest.get(quote)?;
            Some(EvalCtx {
                value: b.value.checked_div(q.value)?,
                conf_ratio: max_conf_ratio([b.conf_ratio, q.conf_ratio]),
                publish_time_us: older(b.publish_time_us, q.publish_time_us),
                slot: None,
                source: b.source,
                derived: true,
            })
        }
        BaseState::Basket {
            legs,
            rebase,
            factor,
        } => {
            let mut sum = Decimal::ZERO;
            let mut ratios = Vec::with_capacity(legs.len());
            let mut publish = None;
            for (feed, weight) in legs.iter() {
                let snap = latest.get(feed)?;
                sum = sum.checked_add(snap.value.checked_mul(*weight)?)?;
                ratios.push(snap.conf_ratio);
                publish = older(publish, snap.publish_time_us);
            }
            if factor.is_none() {
                *factor = Some(match rebase {
                    Some(target) => target.checked_div(sum)?,
                    None => Decimal::ONE,
                });
            }
            Some(EvalCtx {
                value: sum.checked_mul((*factor)?)?,
                conf_ratio: max_conf_ratio(ratios),
                publish_time_us: publish,
                slot: None,
                source: PriceSource::Derived,
                derived: true,
            })
        }
        BaseState::Peg { target } => Some(EvalCtx {
            value: *target,
            conf_ratio: None,
            publish_time_us: Some(now_us),
            slot: None,
            source: PriceSource::Derived,
            derived: true,
        }),
        BaseState::Gbm {
            price,
            sigma_daily,
            drift_daily,
            tick_ms,
            rng,
        } => {
            let (sigma, drift) = (*sigma_daily, *drift_daily);
            let dt_days = (*tick_ms as f64) / 86_400_000.0;
            let z: f64 = StandardNormal.sample(rng.as_mut());
            *price *= ((drift - 0.5 * sigma * sigma) * dt_days + sigma * dt_days.sqrt() * z).exp();
            let value = Decimal::from_f64(*price)?;
            let conf = Decimal::from_f64(sigma * dt_days.sqrt())?;
            Some(EvalCtx {
                value,
                conf_ratio: Some(conf),
                publish_time_us: Some(now_us),
                slot: None,
                source: PriceSource::Derived,
                derived: true,
            })
        }
    }
}

fn apply_transform(transform: &mut TransformState, ctx: EvalCtx, now_us: i64) -> Option<EvalCtx> {
    match transform {
        TransformState::Scale { factor, offset } => Some(EvalCtx {
            value: ctx.value.checked_mul(*factor)?.checked_add(*offset)?,
            derived: true,
            ..ctx
        }),
        TransformState::Invert => Some(EvalCtx {
            value: Decimal::ONE.checked_div(ctx.value)?,
            derived: true,
            ..ctx
        }),
        TransformState::Beta {
            beta,
            anchor,
            initial,
        } => {
            let ratio = ctx.value.to_f64()? / *anchor;
            if ratio <= 0.0 {
                return None;
            }
            let out = *initial * ratio.powf(*beta);
            Some(EvalCtx {
                value: Decimal::from_f64(out)?,
                derived: true,
                ..ctx
            })
        }
        TransformState::Lag { us, buf } => {
            buf.push_back(LagSample {
                at_us: now_us,
                ctx: ctx.clone(),
            });
            let cutoff = now_us - *us;
            let mut chosen = None;
            while buf.front().is_some_and(|s| s.at_us <= cutoff) {
                chosen = buf.pop_front();
            }
            chosen.map(|sample| EvalCtx {
                derived: true,
                ..sample.ctx
            })
        }
        TransformState::Noise {
            sigma,
            halflife_us,
            x,
            last_us,
            rng,
        } => {
            let z: f64 = StandardNormal.sample(rng.as_mut());
            match last_us {
                None => {
                    // start at the stationary distribution
                    *x = *sigma * z;
                }
                Some(last) => {
                    let dt = (now_us - *last).max(0) as f64;
                    let tau = *halflife_us / std::f64::consts::LN_2;
                    let a = (-dt / tau).exp();
                    *x = *x * a + *sigma * (1.0 - a * a).sqrt() * z;
                }
            }
            *last_us = Some(now_us);
            let mult = Decimal::from_f64(x.exp())?;
            Some(EvalCtx {
                value: ctx.value.checked_mul(mult)?,
                derived: true,
                ..ctx
            })
        }
    }
}

fn older(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (x, None) | (None, x) => x,
    }
}

fn max_conf_ratio(ratios: impl IntoIterator<Item = Option<Decimal>>) -> Option<Decimal> {
    ratios.into_iter().flatten().max()
}

/// Engine task: consumes underlying ticks, drives the clock, publishes into
/// the shared state.
pub async fn run(mut engine: Engine, mut ticks: mpsc::Receiver<PricePoint>, state: Arc<AppState>) {
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(ENGINE_TICK_MS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            maybe = ticks.recv() => match maybe {
                Some(point) => {
                    let now = mtm_common::time::now_us();
                    for out in engine.on_underlying(&point, now) {
                        state.update(out).await;
                    }
                }
                None => break,
            },
            _ = interval.tick() => {
                let now = mtm_common::time::now_us();
                for out in engine.on_tick(now) {
                    state.update(out).await;
                }
            }
        }
    }
    warn!("pricing engine input channel closed, engine stopping");
}

#[cfg(test)]
mod tests;
