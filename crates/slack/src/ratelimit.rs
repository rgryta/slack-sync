//! An async token bucket, shared by every call in the process.
//!
//! `conversations.history` is documented Tier 3 ("50+ per minute"). The May 2025
//! tightening to one request per minute targets non-Marketplace *apps*, and a session
//! token is not an app — but that is an inference from the announcement, not a
//! guarantee, so the default rate sits far below Tier 3 and this type is the single
//! place to slow down if reality disagrees.
//!
//! This is deliberately not a general-purpose rate limiter. `reserve` is what lets a
//! backfill yield to a poll pass rather than split the budget fairly with it, and no
//! off-the-shelf limiter has it.

use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::errors::SlackError;

struct State {
    tokens: f64,
    updated: Instant,
}

/// `rate` tokens per second, at most `burst` held at once.
///
/// Time comes from `tokio::time::Instant`, which is monotonic, so a clock step cannot
/// hand out a windfall of tokens or stall the bucket for hours.
pub struct TokenBucket {
    rate: f64,
    burst: f64,
    state: Mutex<State>,
    /// Counted only under `cfg(test)`: it is how a test sees the drain a 429 causes
    /// without having to observe the bucket's level on a clock.
    #[cfg(test)]
    drains: std::sync::atomic::AtomicUsize,
    /// Likewise: the reserves the bucket was actually asked for, so a test can see that
    /// a background caller's `reserve` reached here rather than being dropped on the way.
    #[cfg(test)]
    reserves: std::sync::Mutex<Vec<f64>>,
}

impl TokenBucket {
    pub fn new(rate: f64, burst: u32) -> Result<Self, SlackError> {
        if rate <= 0.0 || rate.is_nan() {
            return Err(SlackError::Config(format!("rate must be positive: {rate}")));
        }
        if burst < 1 {
            return Err(SlackError::Config("burst must be at least 1".to_owned()));
        }
        let burst = f64::from(burst);
        Ok(Self {
            rate,
            burst,
            state: Mutex::new(State {
                tokens: burst,
                updated: Instant::now(),
            }),
            #[cfg(test)]
            drains: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            reserves: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn refill(&self, state: &mut State) {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(state.updated).as_secs_f64();
        state.updated = now;
        state.tokens = (state.tokens + elapsed * self.rate).min(self.burst);
    }

    /// Wait until a token is available, then spend it.
    ///
    /// The lock is held across the sleep so waiters are served in arrival order and two
    /// callers cannot both decide the same token is theirs. `tokio::sync::Mutex` is
    /// fair, which is what makes that true rather than merely likely.
    ///
    /// `reserve` is how much of the bucket the caller agrees to leave alone: it waits
    /// for `1 + reserve` tokens but still spends only one. Arrival order alone is fair,
    /// and fair is wrong here — a continuously-running backfill would take half the
    /// budget and double the time a poll pass needs. A background caller passing a
    /// reserve yields to a foreground one that passes none, without a scheduler.
    ///
    /// The reserve is capped below `burst`, because a reserve the bucket can never hold
    /// would block the caller forever rather than deprioritise it.
    pub async fn acquire(&self, reserve: f64) {
        #[cfg(test)]
        self.reserves.lock().expect("the reserve log").push(reserve);
        let headroom = 1.0 + reserve.min(self.burst - 1.0);
        let mut state = self.state.lock().await;
        self.refill(&mut state);
        if state.tokens < headroom {
            let wait = (headroom - state.tokens) / self.rate;
            tokio::time::sleep(Duration::from_secs_f64(wait)).await;
            self.refill(&mut state);
        }
        state.tokens -= 1.0;
    }

    /// Empty the bucket.
    ///
    /// Called on a 429 so every concurrent caller backs off, rather than each one
    /// discovering the same rate limit for itself.
    pub async fn drain(&self) {
        #[cfg(test)]
        self.drains
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut state = self.state.lock().await;
        self.refill(&mut state);
        state.tokens = 0.0;
    }

    #[cfg(test)]
    pub(crate) fn drains(&self) -> usize {
        self.drains.load(std::sync::atomic::Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn reserves(&self) -> Vec<f64> {
        self.reserves.lock().expect("the reserve log").clone()
    }
}

/// Time is virtual in every test here (`start_paused`), so nothing sleeps for real and
/// the elapsed figures are exact rather than approximate.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_settings_are_rejected() {
        for (rate, burst) in [(0.0, 1), (-1.0, 1), (1.0, 0)] {
            assert!(
                TokenBucket::new(rate, burst).is_err(),
                "rate {rate}, burst {burst}"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn burst_is_free_then_the_rate_applies() {
        let bucket = TokenBucket::new(2.0, 3).unwrap();
        let start = Instant::now();
        for _ in 0..3 {
            bucket.acquire(0.0).await;
        }
        assert_eq!(start.elapsed(), Duration::ZERO);

        bucket.acquire(0.0).await;
        assert_eq!(start.elapsed(), Duration::from_millis(500));
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_callers_are_serialised() {
        // Five callers through a one-token bucket cost four intervals, not zero.
        let bucket = TokenBucket::new(4.0, 1).unwrap();
        let start = Instant::now();
        tokio::join!(
            bucket.acquire(0.0),
            bucket.acquire(0.0),
            bucket.acquire(0.0),
            bucket.acquire(0.0),
            bucket.acquire(0.0),
        );
        assert_eq!(start.elapsed(), Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn tokens_refill_over_time_but_do_not_bank() {
        let bucket = TokenBucket::new(1.0, 2).unwrap();
        bucket.acquire(0.0).await;
        bucket.acquire(0.0).await;
        tokio::time::advance(Duration::from_secs(10)).await;

        let start = Instant::now();
        // Refill is capped at `burst`; idle time does not bank an unbounded credit.
        bucket.acquire(0.0).await;
        bucket.acquire(0.0).await;
        assert_eq!(start.elapsed(), Duration::ZERO);
        bucket.acquire(0.0).await;
        assert_eq!(start.elapsed(), Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn drain_makes_every_caller_wait() {
        // A 429 must slow everyone down, not just the caller that saw it.
        let bucket = TokenBucket::new(2.0, 5).unwrap();
        bucket.drain().await;
        let start = Instant::now();
        bucket.acquire(0.0).await;
        assert_eq!(start.elapsed(), Duration::from_millis(500));
    }

    #[tokio::test(start_paused = true)]
    async fn a_reserve_yields_to_a_caller_without_one() {
        let bucket = TokenBucket::new(1.0, 4).unwrap();
        let start = Instant::now();
        bucket.acquire(0.0).await; // foreground spends one, 3 left
        assert_eq!(start.elapsed(), Duration::ZERO);

        // Background wants 1 + 2 reserved = 3, and exactly 3 are there.
        bucket.acquire(2.0).await;
        assert_eq!(start.elapsed(), Duration::ZERO);

        // Now 2 remain, below the background caller's floor, so it waits...
        bucket.acquire(2.0).await;
        assert_eq!(start.elapsed(), Duration::from_secs(1));

        // ...while the foreground caller spends what is left without waiting.
        let resumed = Instant::now();
        bucket.acquire(0.0).await;
        assert_eq!(resumed.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn a_reserve_larger_than_the_bucket_still_lets_the_caller_through() {
        // A floor the bucket can never reach would block forever rather than
        // deprioritise.
        let bucket = TokenBucket::new(1.0, 2).unwrap();
        let start = Instant::now();
        bucket.acquire(99.0).await;
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}
