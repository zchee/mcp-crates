//! Lock-free request pacing.
//!
//! crates.io asks API clients to stay at or below one request per second. This
//! module implements that budget as a virtual-scheduling pacer (the same idea as
//! GCRA): every caller atomically claims the next transmission slot on a shared
//! timeline and then sleeps until that slot arrives.
//!
//! The properties that matter here:
//!
//! * **One atomic read-modify-write per request.** There is no mutex, no background timer
//!   task, and no channel, so the pacer adds no scheduling overhead of its own.
//! * **Arrival-ordered fairness.** Slots are handed out in the order callers reach the
//!   compare-exchange, so no request can be starved by later arrivals.
//! * **Bounded queueing.** A caller that would have to wait beyond the configured ceiling
//!   is rejected immediately instead of stalling a tool call for an unbounded time.

use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use tokio::time::Instant;

/// Outcome of reserving a slot.
#[derive(Debug)]
enum Reservation {
    /// The slot is now or in the past; send immediately.
    Immediate,
    /// Sleep until this instant, then send.
    At(Instant),
}

/// A virtual-scheduling rate limiter for a single upstream host.
#[derive(Debug)]
pub struct Pacer {
    /// Origin of the internal timeline.
    base: Instant,
    /// Minimum spacing between two consecutive requests, in nanoseconds.
    interval_nanos: u64,
    /// Nanoseconds (relative to [`Pacer::base`]) at which the next slot opens.
    next_slot_nanos: AtomicU64,
    /// Longest a caller may be asked to wait before the request is shed.
    max_wait_nanos: u64,
    /// Host label used in diagnostics.
    host: &'static str,
}

impl Pacer {
    /// Create a pacer that emits at most one request per `min_interval` and
    /// sheds callers that would have to wait longer than `max_wait`.
    #[must_use]
    pub fn new(host: &'static str, min_interval: Duration, max_wait: Duration) -> Self {
        Self {
            base: Instant::now(),
            interval_nanos: u64::try_from(min_interval.as_nanos()).unwrap_or(u64::MAX),
            next_slot_nanos: AtomicU64::new(0),
            max_wait_nanos: u64::try_from(max_wait.as_nanos()).unwrap_or(u64::MAX),
            host,
        }
    }

    /// Nanoseconds elapsed on the internal timeline.
    fn now_nanos(&self) -> u64 {
        u64::try_from(self.base.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    /// Claim the next slot without blocking, or report that the queue is full.
    fn reserve(&self) -> Result<Reservation, crate::Error> {
        let now = self.now_nanos();
        let mut observed = self.next_slot_nanos.load(Ordering::Acquire);
        loop {
            let slot = observed.max(now);
            let wait = slot - now;
            if wait > self.max_wait_nanos {
                return Err(crate::Error::RateLimitQueueFull {
                    host: self.host.to_owned(),
                    queued_ms: wait / 1_000_000,
                    ceiling_ms: self.max_wait_nanos / 1_000_000,
                });
            }
            match self.next_slot_nanos.compare_exchange_weak(
                observed,
                slot.saturating_add(self.interval_nanos),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) if wait == 0 => return Ok(Reservation::Immediate),
                Ok(_) => return Ok(Reservation::At(self.base + Duration::from_nanos(slot))),
                Err(actual) => observed = actual,
            }
        }
    }

    /// Wait until this caller's slot on the shared timeline arrives.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::RateLimitQueueFull`] when the backlog already
    /// extends past the configured ceiling.
    pub async fn acquire(&self) -> Result<(), crate::Error> {
        match self.reserve()? {
            Reservation::Immediate => Ok(()),
            Reservation::At(deadline) => {
                tokio::time::sleep_until(deadline).await;
                Ok(())
            },
        }
    }

    /// Push the whole timeline forward, e.g. after a `429` or a `Retry-After`.
    ///
    /// Every caller that has not yet been served is delayed, and slots already
    /// claimed further in the future are left untouched.
    pub fn penalize(&self, backoff: Duration) {
        let target = self
            .now_nanos()
            .saturating_add(u64::try_from(backoff.as_nanos()).unwrap_or(u64::MAX));
        self.next_slot_nanos.fetch_max(target, Ordering::AcqRel);
    }

    /// How far the current backlog extends into the future.
    #[must_use]
    pub fn backlog(&self) -> Duration {
        let now = self.now_nanos();
        let next = self.next_slot_nanos.load(Ordering::Acquire);
        Duration::from_nanos(next.saturating_sub(now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn spaces_requests_by_the_configured_interval() {
        let pacer = Pacer::new("test", Duration::from_secs(1), Duration::from_secs(60));
        let start = Instant::now();

        for _ in 0..4 {
            pacer.acquire().await.expect("queue has room");
        }

        // Four requests at one per second: the first goes out immediately, so
        // three intervals have elapsed once the fourth is released.
        assert_eq!(start.elapsed(), Duration::from_secs(3));
    }

    #[tokio::test(start_paused = true)]
    async fn sheds_callers_beyond_the_queue_ceiling() {
        let pacer = Pacer::new("test", Duration::from_secs(1), Duration::from_secs(2));

        // Reserved without waiting, so the backlog builds up rather than
        // draining as it would if each caller slept for its slot.
        for expected_wait in 0..3 {
            pacer.reserve().unwrap_or_else(|err| {
                panic!("slot {expected_wait}s out should fit under the ceiling: {err}")
            });
        }

        // The fourth would land at t=3, one second past the ceiling.
        let err = pacer.reserve().expect_err("queue is saturated");
        assert!(
            matches!(err, crate::Error::RateLimitQueueFull { .. }),
            "got {err:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn penalize_delays_subsequent_callers() {
        let pacer = Pacer::new("test", Duration::from_millis(10), Duration::from_secs(60));
        let start = Instant::now();

        pacer.penalize(Duration::from_secs(5));
        pacer.acquire().await.expect("queue has room");

        assert_eq!(start.elapsed(), Duration::from_secs(5));
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_callers_are_each_given_a_distinct_slot() {
        let pacer = std::sync::Arc::new(Pacer::new(
            "test",
            Duration::from_secs(1),
            Duration::from_secs(60),
        ));
        let start = Instant::now();

        let mut handles = Vec::new();
        for _ in 0..8 {
            let pacer = pacer.clone();
            handles.push(tokio::spawn(async move { pacer.acquire().await }));
        }
        for handle in handles {
            handle.await.expect("task joins").expect("queue has room");
        }

        // Eight slots at one per second means the last opens at t=7.
        assert_eq!(start.elapsed(), Duration::from_secs(7));
        assert_eq!(pacer.backlog(), Duration::from_secs(1));
    }
}
