//! The wait before a frozen snapshot reads: in-flight transactions may still
//! land until their blockhash expires, 150 blocks after it was fetched. On a
//! remote cluster the wait is counted in blocks, so a slow or skipping
//! cluster gets the time it needs; a fixed wall-clock wait is the fallback
//! (and the local default, where a slot or two is enough).
//!
//! A failed height poll is retried (until the timeout), never taken as a
//! reason to stop counting blocks. A timeout does not fail the snapshot: the
//! window it closes has already run, so the snapshot is written with its
//! [`Mode`] recorded, and `report` warns about it.

use std::future::Future;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// How long a block-based wait may take before the snapshot is taken anyway
/// (and marked [`Mode::TimedOut`]).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);
/// Block height polls while waiting.
pub const POLL: Duration = Duration::from_secs(1);

/// How a settle wait ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Mode {
    /// The block height advanced the blocks asked for.
    Blocks,
    /// A wall-clock wait was asked for (`blocks == 0`, the local default).
    WallClock,
    /// Blocks were asked for, but the height was never readable: the
    /// wall-clock fallback ran instead.
    Fallback,
    /// The height did not advance enough before the timeout: in-flight
    /// transactions may still land after the snapshot.
    TimedOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settled {
    pub mode: Mode,
    /// Blocks the cluster advanced (`None`: the height was never read).
    pub blocks: Option<u64>,
    pub waited: Duration,
}

/// What a snapshot records about its settle wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    pub mode: Mode,
    pub blocks: Option<u64>,
    pub waited_ms: u64,
}

impl From<Settled> for Record {
    fn from(settled: Settled) -> Self {
        Self {
            mode: settled.mode,
            blocks: settled.blocks,
            waited_ms: u64::try_from(settled.waited.as_millis()).unwrap_or(u64::MAX),
        }
    }
}

/// Waits until `height` has advanced by `blocks` from its first reading.
/// `blocks == 0` sleeps `fallback` instead ([`Mode::WallClock`]). Failed
/// polls are retried every `poll`: before the first reading for up to
/// `fallback` (then [`Mode::Fallback`], the wall-clock wait having run),
/// after it until `timeout` ([`Mode::TimedOut`], with the blocks seen).
pub async fn settle<F, Fut>(
    mut height: F,
    blocks: u64,
    fallback: Duration,
    timeout: Duration,
    poll: Duration,
) -> Result<Settled>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<u64>>,
{
    let started = Instant::now();
    if blocks == 0 {
        tokio::time::sleep(fallback).await;
        return Ok(Settled {
            mode: Mode::WallClock,
            blocks: None,
            waited: started.elapsed(),
        });
    }
    let mut from = None;
    let mut advanced = 0;
    loop {
        match height().await {
            Ok(now) => {
                let from = *from.get_or_insert(now);
                advanced = now.saturating_sub(from);
                if advanced >= blocks {
                    return Ok(Settled {
                        mode: Mode::Blocks,
                        blocks: Some(advanced),
                        waited: started.elapsed(),
                    });
                }
            }
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "block height unavailable: retrying");
                if from.is_none() && started.elapsed() >= fallback {
                    tracing::warn!("block height never readable: settled on wall-clock time");
                    return Ok(Settled {
                        mode: Mode::Fallback,
                        blocks: None,
                        waited: started.elapsed(),
                    });
                }
            }
        }
        if started.elapsed() >= timeout {
            tracing::warn!(
                advanced,
                blocks,
                "the cluster did not advance enough blocks before the timeout: in-flight transactions may not have settled"
            );
            return Ok(Settled {
                mode: Mode::TimedOut,
                blocks: from.map(|_| advanced),
                waited: started.elapsed(),
            });
        }
        tokio::time::sleep(poll).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use anyhow::anyhow;

    use super::*;

    const MS: Duration = Duration::from_millis(1);

    /// Heights handed out in order; the last one repeats.
    fn heights(values: Vec<Result<u64>>) -> impl FnMut() -> std::future::Ready<Result<u64>> {
        let values = Mutex::new(
            values
                .into_iter()
                .collect::<std::collections::VecDeque<_>>(),
        );
        move || {
            let mut values = values.lock().expect("lock");
            let next = if values.len() > 1 {
                values.pop_front().expect("value")
            } else {
                match values.front().expect("value") {
                    Ok(height) => Ok(*height),
                    Err(error) => Err(anyhow!("{error}")),
                }
            };
            std::future::ready(next)
        }
    }

    #[tokio::test]
    async fn returns_only_once_the_height_advanced_enough() {
        let settled = settle(
            heights(vec![Ok(100), Ok(100), Ok(120), Ok(149), Ok(250), Ok(400)]),
            150,
            Duration::from_secs(60),
            Duration::from_secs(5),
            MS,
        )
        .await
        .expect("settled");
        // 250 is the first reading 150 past the start, and far sooner than
        // the 60 s wall-clock fallback.
        assert_eq!(settled.mode, Mode::Blocks);
        assert_eq!(settled.blocks, Some(150));
        assert!(settled.waited < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn a_failed_poll_is_retried_not_a_fallback() {
        let settled = settle(
            heights(vec![Ok(100), Err(anyhow!("throttled")), Ok(260)]),
            150,
            Duration::ZERO,
            Duration::from_secs(5),
            MS,
        )
        .await
        .expect("settled");
        assert_eq!(settled.mode, Mode::Blocks);
        assert_eq!(settled.blocks, Some(160));

        // A failing first read is retried too: the count starts at the
        // first height that could be read.
        let settled = settle(
            heights(vec![Err(anyhow!("throttled")), Ok(100), Ok(250)]),
            150,
            Duration::from_secs(60),
            Duration::from_secs(5),
            MS,
        )
        .await
        .expect("settled");
        assert_eq!((settled.mode, settled.blocks), (Mode::Blocks, Some(150)));
    }

    #[tokio::test]
    async fn a_stalled_cluster_times_out_but_still_settles() {
        let settled = settle(
            heights(vec![Ok(100), Ok(101)]),
            150,
            Duration::ZERO,
            20 * MS,
            MS,
        )
        .await
        .expect("a finished window is kept");
        assert_eq!(settled.mode, Mode::TimedOut);
        assert_eq!(settled.blocks, Some(1));
        assert!(settled.waited >= 20 * MS);

        // Heights failing after the first reading: still a timeout, never
        // a silent wall-clock settle.
        let settled = settle(
            heights(vec![Ok(100), Err(anyhow!("down"))]),
            150,
            MS,
            20 * MS,
            MS,
        )
        .await
        .expect("settled");
        assert_eq!((settled.mode, settled.blocks), (Mode::TimedOut, Some(0)));
    }

    #[tokio::test]
    async fn zero_blocks_or_no_height_ever_falls_back_to_wall_clock() {
        let settled = settle(heights(vec![Ok(1)]), 0, 30 * MS, Duration::ZERO, MS)
            .await
            .expect("settled");
        assert_eq!((settled.mode, settled.blocks), (Mode::WallClock, None));
        assert!(settled.waited >= 30 * MS);

        let settled = settle(
            heights(vec![Err(anyhow!("down"))]),
            150,
            30 * MS,
            Duration::from_secs(5),
            MS,
        )
        .await
        .expect("settled");
        assert_eq!((settled.mode, settled.blocks), (Mode::Fallback, None));
        assert!(settled.waited >= 30 * MS && settled.waited < Duration::from_secs(5));
    }

    #[test]
    fn the_record_serializes_its_mode() {
        let record = Record::from(Settled {
            mode: Mode::TimedOut,
            blocks: Some(3),
            waited: Duration::from_millis(1500),
        });
        assert_eq!(
            serde_json::to_value(record).expect("json"),
            serde_json::json!({ "mode": "timedOut", "blocks": 3, "waitedMs": 1500 })
        );
    }
}
