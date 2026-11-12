//! The wait before a frozen snapshot reads: in-flight transactions may still
//! land until their blockhash expires, 150 blocks after it was fetched. On a
//! remote cluster the wait is counted in blocks, so a slow or skipping
//! cluster gets the time it needs; a fixed wall-clock wait is the fallback
//! (and the local default, where a slot or two is enough).

use std::future::Future;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

/// How long a block-based wait may take before the snapshot fails.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);
/// Block height polls while waiting.
pub const POLL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settled {
    /// Blocks the cluster advanced (`None`: the wall-clock fallback ran).
    pub blocks: Option<u64>,
    pub waited: Duration,
}

/// Waits until `height` has advanced by `blocks` from its first reading.
/// `blocks == 0`, or a block height that cannot be read, sleeps `fallback`
/// instead (only what is left of it, if the height failed mid-wait). A
/// cluster that does not advance `blocks` within `timeout` is an error: the
/// snapshot would read before everything in flight had settled.
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
    let wall_clock = |started: Instant| async move {
        tokio::time::sleep(fallback.saturating_sub(started.elapsed())).await;
        Ok(Settled {
            blocks: None,
            waited: started.elapsed(),
        })
    };
    if blocks == 0 {
        return wall_clock(started).await;
    }
    let from = match height().await {
        Ok(from) => from,
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), "block height unavailable: settling on wall-clock time");
            return wall_clock(started).await;
        }
    };
    loop {
        tokio::time::sleep(poll).await;
        let now = match height().await {
            Ok(now) => now,
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "block height unavailable: settling on wall-clock time");
                return wall_clock(started).await;
            }
        };
        let advanced = now.saturating_sub(from);
        if advanced >= blocks {
            return Ok(Settled {
                blocks: Some(advanced),
                waited: started.elapsed(),
            });
        }
        if started.elapsed() >= timeout {
            bail!(
                "the cluster advanced {advanced} of {blocks} blocks in {} s: in-flight transactions may not have settled",
                started.elapsed().as_secs()
            );
        }
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
        assert_eq!(settled.blocks, Some(150));
        assert!(settled.waited < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn a_stalled_cluster_errors_at_the_timeout() {
        let error = settle(
            heights(vec![Ok(100), Ok(101)]),
            150,
            Duration::ZERO,
            20 * MS,
            MS,
        )
        .await
        .expect_err("timed out");
        assert!(
            format!("{error}").contains("advanced 1 of 150 blocks"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn zero_blocks_or_no_height_falls_back_to_wall_clock() {
        let settled = settle(heights(vec![Ok(1)]), 0, 30 * MS, Duration::ZERO, MS)
            .await
            .expect("settled");
        assert_eq!(settled.blocks, None);
        assert!(settled.waited >= 30 * MS);

        let settled = settle(
            heights(vec![Ok(100), Err(anyhow!("down"))]),
            150,
            30 * MS,
            Duration::from_secs(5),
            MS,
        )
        .await
        .expect("settled");
        assert_eq!(settled.blocks, None);
        assert!(settled.waited >= 30 * MS && settled.waited < Duration::from_secs(5));
    }
}
