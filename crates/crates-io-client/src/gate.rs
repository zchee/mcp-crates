//! Per-key serialization of expensive work.
//!
//! When several callers want the same thing at the same time, letting them all
//! do the work means paying for it several times over and, for anything that
//! leaves the process, spending several times the request budget. A gate lets
//! the first caller through and holds the rest until it is done, so the work
//! happens once and everyone reads the result.

use std::{sync::Arc, time::Duration};

use moka::future::Cache;
use tokio::sync::Mutex;

/// How long an unused gate is kept before it is dropped.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// A set of gates, one per key.
#[derive(Debug)]
pub struct Gates {
    gates: Cache<Arc<str>, Arc<Mutex<()>>>,
}

impl Gates {
    /// Build a gate set holding at most `capacity` gates.
    #[must_use]
    pub fn new(capacity: u64) -> Self {
        Self {
            // Bounded by count and expiring when idle: a gate exists only to
            // serialize callers that overlap, so one nobody is waiting on is
            // just memory.
            gates: Cache::builder().max_capacity(capacity).time_to_idle(IDLE_TIMEOUT).build(),
        }
    }

    /// The gate for a key.
    ///
    /// Lock the returned mutex to enter. Eviction under pressure can hand two
    /// callers different gates for the same key, which costs a duplicated piece
    /// of work but never a wrong answer.
    pub async fn get(&self, key: &Arc<str>) -> Arc<Mutex<()>> {
        self.gates.get_with(Arc::clone(key), async { Arc::new(Mutex::new(())) }).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[tokio::test]
    async fn the_same_key_yields_the_same_gate() {
        let gates = Gates::new(16);
        let key: Arc<str> = Arc::from("k");

        let first = gates.get(&key).await;
        let second = gates.get(&Arc::from("k")).await;

        assert!(Arc::ptr_eq(&first, &second), "an equal key must map to one gate");
    }

    #[tokio::test]
    async fn different_keys_do_not_block_each_other() {
        let gates = Gates::new(16);
        let held = gates.get(&Arc::from("a")).await;
        let _guard = held.lock().await;

        // Would deadlock if keys shared a gate.
        let other = gates.get(&Arc::from("b")).await;
        assert!(other.try_lock().is_ok());
    }

    #[tokio::test]
    async fn concurrent_callers_run_the_work_once() {
        let gates = Arc::new(Gates::new(16));
        let done = Arc::new(AtomicUsize::new(0));
        let runs = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..8 {
            let gates = Arc::clone(&gates);
            let done = Arc::clone(&done);
            let runs = Arc::clone(&runs);
            handles.push(tokio::spawn(async move {
                let key: Arc<str> = Arc::from("shared");
                let gate = gates.get(&key).await;
                let _guard = gate.lock().await;
                // The pattern callers use: re-check whether someone already did
                // the work before doing it.
                if done.load(Ordering::SeqCst) == 0 {
                    runs.fetch_add(1, Ordering::SeqCst);
                    tokio::task::yield_now().await;
                    done.store(1, Ordering::SeqCst);
                }
            }));
        }
        for handle in handles {
            handle.await.expect("the task joins");
        }

        assert_eq!(runs.load(Ordering::SeqCst), 1, "eight callers should have done the work once");
    }
}
