//! Pool snapshot cache: single flight, detached loads, stale-while-revalidate.
//!
//! A pool's snapshot is fresh for `ttl` after its load *completed*. Past that
//! the next caller starts a background refresh and every caller keeps getting
//! the last good snapshot until it is `max_age` old, counted from when its
//! load *started* (the data is that old); only then do callers wait
//! for the refresh, and each waits at most its own bound, unless the pool's
//! last load failed: then they get that failure at once. Loads run in their
//! own task, so a caller that is cancelled (request timeout, client gone)
//! never cancels the load others are waiting on. Every load is bounded by
//! `load_timeout`, and a failed load is cached for one `ttl` (at least
//! [`MIN_FAILURE_BACKOFF`]) so a dead pool costs one attempt per backoff, not
//! one per quote.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use mtm_chain::ChainClient;
use mtm_venues::PoolSnapshot;
use solana_pubkey::Pubkey;
use tokio::time::Instant;
use tracing::warn;

use crate::Snapshot;
use crate::registry::Entry;

/// Loads one pool. Production loads go through the venue clients
/// ([`chain_loader`]); tests inject fakes.
pub type Loader<V> = Arc<dyn Fn(Entry) -> BoxFuture<'static, Result<V, String>> + Send + Sync>;

/// The production loader: a venue snapshot over `chain`.
pub fn chain_loader(chain: ChainClient) -> Loader<Snapshot> {
    Arc::new(move |entry: Entry| {
        let chain = chain.clone();
        async move {
            PoolSnapshot::load(&chain, entry.kind, entry.program_id, entry.pool)
                .await
                .map(Arc::new)
                .map_err(|error| error.to_string())
        }
        .boxed()
    })
}

#[derive(Debug, Clone, Copy)]
pub struct CacheTimings {
    /// A snapshot younger than this is served without a refresh.
    pub ttl: Duration,
    /// A snapshot whose load started longer ago than this is never served.
    pub max_age: Duration,
    /// Each load is abandoned (and counted as failed) after this long.
    pub load_timeout: Duration,
}

/// Shortest time a failed pool waits before its next load, so `ttl = 0`
/// (refresh on every quote) cannot reload a dead pool back to back.
pub const MIN_FAILURE_BACKOFF: Duration = Duration::from_millis(200);

impl CacheTimings {
    /// How long a failure suppresses reloads: one `ttl`, at least
    /// [`MIN_FAILURE_BACKOFF`].
    pub fn failure_backoff(&self) -> Duration {
        self.ttl.max(MIN_FAILURE_BACKOFF)
    }
}

/// A snapshot, when its load started (its data is no older) and when it
/// completed.
#[derive(Debug, Clone)]
pub struct Loaded<V> {
    pub value: V,
    pub started_at: Instant,
    pub loaded_at: Instant,
}

impl<V> Loaded<V> {
    /// Age of the data: counted from the load's start, since its first RPC
    /// read may be that old. `max_age` and the age metric use this.
    pub fn age(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.started_at)
    }

    /// Time since the load completed: the TTL counts from here, so a slow
    /// load is not already due for a refresh when it lands.
    pub fn since_loaded(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.loaded_at)
    }
}

type InFlight<V> = Shared<BoxFuture<'static, Result<Loaded<V>, String>>>;

struct PoolCache<V> {
    good: Option<Loaded<V>>,
    /// Last failure and when it happened; suppresses reloads for one
    /// failure backoff.
    failure: Option<(Instant, String)>,
    refresh: Option<InFlight<V>>,
}

impl<V> Default for PoolCache<V> {
    fn default() -> Self {
        Self {
            good: None,
            failure: None,
            refresh: None,
        }
    }
}

type Entries<V> = Arc<Mutex<HashMap<Pubkey, PoolCache<V>>>>;

/// Pool snapshots shared by concurrent quotes; hits hand out the shared
/// snapshot (`Arc`), never a copy.
pub struct SnapshotCache<V = Snapshot> {
    timings: CacheTimings,
    loader: Loader<V>,
    // Never held across an await.
    entries: Entries<V>,
}

enum Next<V> {
    Ready(Result<Loaded<V>, String>),
    Wait(InFlight<V>),
}

impl<V: Clone + Send + Sync + 'static> SnapshotCache<V> {
    pub fn new(timings: CacheTimings, loader: Loader<V>) -> Self {
        Self {
            timings,
            loader,
            entries: Arc::default(),
        }
    }

    pub fn timings(&self) -> CacheTimings {
        self.timings
    }

    /// The pool's snapshot: fresh or stale-but-usable at once, otherwise the
    /// in-flight load, waited on for at most `wait` (only if the pool's last
    /// load did not fail; a failed pool answers its failure at once). The
    /// returned snapshot may be up to `max_age` old; callers check age again
    /// at use.
    pub async fn get(&self, entry: &Entry, wait: Duration) -> Result<Loaded<V>, String> {
        let inflight = match self.next(entry) {
            Next::Ready(result) => return result,
            Next::Wait(inflight) => inflight,
        };
        match tokio::time::timeout(wait, inflight).await {
            Ok(result) => result,
            Err(_) => Err(format!("snapshot load still running after {wait:?}")),
        }
    }

    fn next(&self, entry: &Entry) -> Next<V> {
        let CacheTimings { ttl, max_age, .. } = self.timings;
        let backoff = self.timings.failure_backoff();
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        let pool = entries.entry(entry.pool).or_default();
        if let Some(good) = &pool.good
            && good.since_loaded(now) < ttl
        {
            metrics::counter!("router_snapshot_cache_total", "result" => "hit").increment(1);
            return Next::Ready(Ok(good.clone()));
        }
        let failure_cached = pool
            .failure
            .as_ref()
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) < backoff);
        if pool.refresh.is_none() && !failure_cached {
            pool.refresh = Some(self.spawn_load(*entry));
        }
        if let Some(good) = &pool.good
            && good.age(now) < max_age
        {
            metrics::counter!("router_snapshot_cache_total", "result" => "stale").increment(1);
            return Next::Ready(Ok(good.clone()));
        }
        metrics::counter!("router_snapshot_cache_total", "result" => "miss").increment(1);
        // Wait only while the pool's last load did not fail (its first load,
        // or a refresh after an idle gap). Once a load has failed, callers get
        // that failure at once while the retry runs in the background:
        // otherwise a pool whose loads keep hanging delays every quote on its
        // pairs by the full wait, on every retry.
        match (&pool.failure, &pool.refresh) {
            (Some((_, error)), _) => Next::Ready(Err(error.clone())),
            (None, Some(inflight)) => Next::Wait(inflight.clone()),
            (None, None) => Next::Ready(Err("no snapshot".to_owned())),
        }
    }

    /// Start a detached load and return a handle any number of callers can
    /// await (or drop) without affecting it. Called with the lock held, so
    /// the write-back cannot run before the handle is stored.
    fn spawn_load(&self, entry: Entry) -> InFlight<V> {
        let started = Instant::now();
        let load = tokio::spawn((self.loader)(entry));
        let entries = Arc::clone(&self.entries);
        let load_timeout = self.timings.load_timeout;
        // The outer task owns the write-back, so it happens even if the load
        // panics or times out, and the pool can be retried after one `ttl`.
        let task = tokio::spawn(async move {
            let mut load = load;
            let result = match tokio::time::timeout(load_timeout, &mut load).await {
                Ok(Ok(Ok(value))) => Ok(Loaded {
                    value,
                    started_at: started,
                    loaded_at: Instant::now(),
                }),
                Ok(Ok(Err(error))) => Err(error),
                Ok(Err(join)) => Err(format!("snapshot load task failed: {join}")),
                Err(_) => {
                    load.abort();
                    Err(format!("snapshot load timed out after {load_timeout:?}"))
                }
            };
            let outcome = if result.is_ok() { "ok" } else { "error" };
            metrics::histogram!("router_snapshot_load_seconds", "result" => outcome)
                .record(started.elapsed().as_secs_f64());
            if let Err(error) = &result {
                // Logged once per load attempt, not once per quote that
                // reuses the cached failure.
                warn!(pool = %entry.pool, error = %error, "pool snapshot failed");
                metrics::counter!("router_snapshot_load_failures_total").increment(1);
            }
            let mut entries = entries.lock().unwrap_or_else(PoisonError::into_inner);
            let pool = entries.entry(entry.pool).or_default();
            pool.refresh = None;
            match &result {
                Ok(loaded) => {
                    pool.good = Some(loaded.clone());
                    pool.failure = None;
                }
                // The last good snapshot stays until it ages out.
                Err(error) => pool.failure = Some((Instant::now(), error.clone())),
            }
            result
        });
        async move {
            task.await
                .unwrap_or_else(|join| Err(format!("snapshot load task failed: {join}")))
        }
        .boxed()
        .shared()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use mtm_venues::VenueKind;

    use super::*;
    use crate::registry::Tier;

    const TTL: Duration = Duration::from_millis(400);
    const MAX_AGE: Duration = Duration::from_millis(3_000);
    const LOAD_TIMEOUT: Duration = Duration::from_millis(2_000);

    fn entry() -> Entry {
        Entry {
            kind: VenueKind::OrcaWhirlpool,
            program_id: Pubkey::new_from_array([1; 32]),
            pool: Pubkey::new_from_array([2; 32]),
            mints: (
                Pubkey::new_from_array([3; 32]),
                Pubkey::new_from_array([4; 32]),
            ),
            tier: Tier::Curated,
        }
    }

    /// A loader whose n-th call (0-based) sleeps `delay(n)` and then returns
    /// `n`, or fails when `fails(n)`.
    struct Fake {
        calls: Arc<AtomicUsize>,
    }

    impl Fake {
        fn cache(
            delay: impl Fn(usize) -> Duration + Send + Sync + 'static,
            fails: impl Fn(usize) -> bool + Send + Sync + 'static,
        ) -> (Self, SnapshotCache<usize>) {
            let timings = CacheTimings {
                ttl: TTL,
                max_age: MAX_AGE,
                load_timeout: LOAD_TIMEOUT,
            };
            Self::with_timings(timings, delay, fails)
        }

        fn with_timings(
            timings: CacheTimings,
            delay: impl Fn(usize) -> Duration + Send + Sync + 'static,
            fails: impl Fn(usize) -> bool + Send + Sync + 'static,
        ) -> (Self, SnapshotCache<usize>) {
            let calls = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&calls);
            let delay = Arc::new(delay);
            let fails = Arc::new(fails);
            let loader: Loader<usize> = Arc::new(move |_entry| {
                let call = counter.fetch_add(1, Ordering::SeqCst);
                let delay = delay(call);
                let fails = fails(call);
                async move {
                    tokio::time::sleep(delay).await;
                    if fails {
                        Err(format!("load {call} failed"))
                    } else {
                        Ok(call)
                    }
                }
                .boxed()
            });
            (Self { calls }, SnapshotCache::new(timings, loader))
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    fn fast(_: usize) -> Duration {
        Duration::from_millis(50)
    }

    fn never(_: usize) -> bool {
        false
    }

    /// ROU-2: the first load hangs far past any request timeout. Callers
    /// keep arriving and giving up after 100 ms (as a request timeout would
    /// cancel them). The load is abandoned at `load_timeout`, the failure is
    /// cached for one TTL, and the next load (fast) serves everyone: the
    /// cache recovers instead of restarting a doomed load per caller.
    #[tokio::test(start_paused = true)]
    async fn cancelled_callers_do_not_livelock_a_slow_load() {
        let (fake, cache) = Fake::cache(
            |call| {
                if call == 0 {
                    Duration::from_secs(60)
                } else {
                    Duration::from_millis(50)
                }
            },
            never,
        );
        let cache = Arc::new(cache);
        let started = Instant::now();
        let mut recovered = None;
        while started.elapsed() < Duration::from_secs(10) {
            let cache = Arc::clone(&cache);
            // A request that is cancelled after 100 ms, like the 408 layer.
            let request = tokio::spawn(async move {
                tokio::time::timeout(
                    Duration::from_millis(100),
                    cache.get(&entry(), Duration::from_secs(60)),
                )
                .await
            });
            if let Ok(Ok(Ok(loaded))) = request.await {
                recovered.get_or_insert((started.elapsed(), loaded.value));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let (at, value) = recovered.expect("the cache recovered");
        // Load 0 times out at 2 s; its failure is cached for one TTL; load 1
        // takes 50 ms.
        assert!(
            at <= LOAD_TIMEOUT + TTL + Duration::from_millis(200),
            "{at:?}"
        );
        assert_eq!(value, 1);
        // One load per TTL at most once recovered, not one per caller.
        assert!(fake.calls() <= 2 + (10_000 / 400) + 1, "{}", fake.calls());
    }

    /// ROU-2: a caller dropped mid-load does not cancel the load.
    #[tokio::test(start_paused = true)]
    async fn dropping_the_caller_keeps_the_load() {
        let (fake, cache) = Fake::cache(fast, never);
        let first = tokio::time::timeout(
            Duration::from_millis(10),
            cache.get(&entry(), Duration::from_secs(1)),
        )
        .await;
        assert!(first.is_err(), "the first caller gave up");
        tokio::time::sleep(Duration::from_millis(100)).await;
        let loaded = cache
            .get(&entry(), Duration::ZERO)
            .await
            .expect("loaded in the background");
        assert_eq!(loaded.value, 0);
        assert_eq!(fake.calls(), 1);
    }

    /// ROU-2: a failure is cached for one TTL, then retried.
    #[tokio::test(start_paused = true)]
    async fn failures_are_cached_for_one_ttl() {
        let (fake, cache) = Fake::cache(fast, |call| call == 0);
        let wait = Duration::from_secs(1);
        assert!(cache.get(&entry(), wait).await.is_err());
        assert!(cache.get(&entry(), wait).await.is_err());
        assert_eq!(fake.calls(), 1, "the failure was reused");
        tokio::time::sleep(TTL).await;
        // The retry starts; the failed pool answers its failure meanwhile.
        assert!(cache.get(&entry(), wait).await.is_err());
        assert_eq!(fake.calls(), 2);
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(cache.get(&entry(), wait).await.expect("retried").value, 1);
        assert_eq!(fake.calls(), 2);
    }

    /// R2-ROUTER-1: a pool whose loads keep hanging delays only the quotes
    /// that arrive before its first load gives up. Once that load has
    /// failed, callers get the failure at once, also while the retry runs.
    #[tokio::test(start_paused = true)]
    async fn a_failed_pool_does_not_make_callers_wait_again() {
        let (fake, cache) = Fake::cache(|_| Duration::from_secs(60), never);
        let wait = Duration::from_secs(1);
        let before = Instant::now();
        assert!(cache.get(&entry(), wait).await.is_err());
        assert_eq!(before.elapsed(), wait, "the first load is waited on");
        // Load 0 times out at LOAD_TIMEOUT; the retry starts one TTL later.
        tokio::time::sleep(LOAD_TIMEOUT + TTL).await;
        for _ in 0..5 {
            let before = Instant::now();
            let error = cache.get(&entry(), wait).await.expect_err("still failing");
            assert!(error.contains("timed out"), "{error}");
            assert_eq!(before.elapsed(), Duration::ZERO, "no wait on a failed pool");
        }
        assert_eq!(fake.calls(), 2, "the retry is in flight");
    }

    /// ROU-3 (a): a load slower than the TTL is fresh for a full TTL after
    /// it completes, not already expired when it lands.
    #[tokio::test(start_paused = true)]
    async fn age_counts_from_load_completion() {
        let (fake, cache) = Fake::cache(|_| Duration::from_millis(1_000), never);
        let wait = Duration::from_secs(2);
        assert_eq!(cache.get(&entry(), wait).await.expect("loaded").value, 0);
        tokio::time::sleep(TTL - Duration::from_millis(1)).await;
        assert_eq!(cache.get(&entry(), wait).await.expect("hit").value, 0);
        assert_eq!(fake.calls(), 1, "no reload within one TTL of completion");
    }

    /// ROU-3 (b): during a refresh, callers get the previous snapshot at once
    /// and the refresh lands in the background.
    #[tokio::test(start_paused = true)]
    async fn stale_snapshot_is_served_while_refreshing() {
        let (fake, cache) = Fake::cache(|_| Duration::from_millis(500), never);
        let wait = Duration::from_secs(1);
        assert_eq!(cache.get(&entry(), wait).await.expect("loaded").value, 0);
        tokio::time::sleep(TTL).await;
        for _ in 0..5 {
            let before = Instant::now();
            let loaded = cache.get(&entry(), wait).await.expect("stale");
            assert_eq!(loaded.value, 0);
            assert_eq!(
                before.elapsed(),
                Duration::ZERO,
                "did not wait for the load"
            );
        }
        assert_eq!(fake.calls(), 2, "one refresh for all stale callers");
        tokio::time::sleep(Duration::from_millis(501)).await;
        assert_eq!(cache.get(&entry(), wait).await.expect("fresh").value, 1);
    }

    /// ROU-3 (c): a snapshot older than `max_age` is not served; callers wait
    /// (bounded) for the refresh instead.
    #[tokio::test(start_paused = true)]
    async fn snapshots_past_max_age_are_not_served() {
        let (_fake, cache) = Fake::cache(
            |call| {
                if call == 0 {
                    Duration::from_millis(10)
                } else {
                    Duration::from_secs(60)
                }
            },
            never,
        );
        assert_eq!(
            cache
                .get(&entry(), Duration::from_secs(1))
                .await
                .expect("loaded")
                .value,
            0
        );
        // Load 1 starts at TTL and hangs; the stale snapshot is served...
        tokio::time::sleep(TTL).await;
        assert!(cache.get(&entry(), Duration::ZERO).await.is_ok());
        // ...until it is `max_age` old. Load 1 has timed out by then, so
        // the pool answers that failure (without waiting).
        tokio::time::sleep(MAX_AGE - TTL).await;
        let before = Instant::now();
        let result = cache.get(&entry(), Duration::from_millis(250)).await;
        assert!(result.is_err(), "{result:?}");
        assert_eq!(before.elapsed(), Duration::ZERO);
    }

    /// R2-ROUTER-3: `max_age` counts from the load's start. A 1.5 s load
    /// lands with data already 1.5 s old; it stops being served 3 s after
    /// the load started, not 3 s after it completed.
    #[tokio::test(start_paused = true)]
    async fn max_age_counts_from_load_start() {
        let load = Duration::from_millis(1_500);
        let (_fake, cache) = Fake::cache(
            move |call| {
                if call == 0 {
                    load
                } else {
                    Duration::from_secs(60)
                }
            },
            never,
        );
        let started = Instant::now();
        let loaded = cache
            .get(&entry(), Duration::from_secs(2))
            .await
            .expect("loaded");
        assert_eq!(loaded.age(Instant::now()), load);
        // Refresh 1 starts one TTL after completion and hangs.
        tokio::time::sleep(MAX_AGE - load - Duration::from_millis(1)).await;
        assert!(cache.get(&entry(), Duration::ZERO).await.is_ok(), "stale");
        tokio::time::sleep(Duration::from_millis(2)).await;
        assert!(started.elapsed() > MAX_AGE);
        let result = cache.get(&entry(), Duration::ZERO).await;
        assert!(result.is_err(), "served past max age: {result:?}");
    }

    /// R2-ROUTER-5: `ttl = 0` still backs a failed pool off, at least
    /// `MIN_FAILURE_BACKOFF` per load instead of one load per quote.
    #[tokio::test(start_paused = true)]
    async fn zero_ttl_keeps_a_failure_backoff() {
        let timings = CacheTimings {
            ttl: Duration::ZERO,
            max_age: MAX_AGE,
            load_timeout: LOAD_TIMEOUT,
        };
        let (fake, cache) = Fake::with_timings(timings, |_| Duration::ZERO, |_| true);
        let window = Duration::from_secs(1);
        let started = Instant::now();
        while started.elapsed() < window {
            assert!(cache.get(&entry(), Duration::from_millis(10)).await.is_err());
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let most = (window.as_millis() / MIN_FAILURE_BACKOFF.as_millis()) as usize + 1;
        assert!(fake.calls() <= most, "{} loads in {window:?}", fake.calls());
    }

    /// A failed refresh keeps the last good snapshot until it ages out.
    #[tokio::test(start_paused = true)]
    async fn failed_refresh_keeps_last_good() {
        let (_fake, cache) = Fake::cache(fast, |call| call == 1);
        let wait = Duration::from_secs(1);
        assert_eq!(cache.get(&entry(), wait).await.expect("loaded").value, 0);
        tokio::time::sleep(TTL).await;
        assert_eq!(cache.get(&entry(), wait).await.expect("stale").value, 0);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            cache.get(&entry(), wait).await.expect("still stale").value,
            0
        );
    }
}
