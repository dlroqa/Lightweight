//! What the router has in flight on each deployment, right now.
//!
//! The only load signal least-busy uses, and one the router can know exactly:
//! it counts its own upstream attempts. A slot is taken when a deployment is
//! actually chosen for an attempt and given back when that attempt is over —
//! by dropping the [`Lease`], never by a call someone has to remember. The
//! lease travels with the attempt: dropped on a pre-commit failure before the
//! next deployment is tried, carried inside a streamed body until the stream
//! ends or the client goes away, and released with a whole body once it has
//! been read. Every exit path — success, error, failover, timeout,
//! cancellation, panic — is a drop.
//!
//! Counted for every policy, so the control API shows real in-flight numbers
//! whichever policy a route uses.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::domain::{DeploymentId, Topology};

/// In-flight counts for every configured deployment.
#[derive(Debug)]
pub struct LoadBook {
    counters: BTreeMap<DeploymentId, Arc<AtomicU64>>,
}

/// One in-flight upstream attempt. Dropping it gives the slot back.
#[derive(Debug)]
#[must_use = "dropping a lease releases the slot at once"]
pub struct Lease {
    counter: Option<Arc<AtomicU64>>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(counter) = self.counter.take() {
            counter.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

impl LoadBook {
    pub fn new(topology: &Topology) -> Self {
        Self {
            counters: topology
                .deployments()
                .iter()
                .map(|deployment| (deployment.id.clone(), Arc::default()))
                .collect(),
        }
    }

    /// Take a slot on `deployment` for one attempt.
    ///
    /// A deployment not in the topology yields a lease that counts nothing,
    /// which validation makes unreachable.
    pub fn acquire(&self, deployment: &DeploymentId) -> Lease {
        let counter = self.counters.get(deployment).map(Arc::clone);
        if let Some(counter) = &counter {
            counter.fetch_add(1, Ordering::AcqRel);
        }
        Lease { counter }
    }

    pub fn active(&self, deployment: &DeploymentId) -> u64 {
        self.counters
            .get(deployment)
            .map_or(0, |counter| counter.load(Ordering::Acquire))
    }

    pub fn snapshot(&self) -> BTreeMap<DeploymentId, u64> {
        self.counters
            .iter()
            .map(|(id, counter)| (id.clone(), counter.load(Ordering::Acquire)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RouterFile, validate};

    fn book() -> (LoadBook, DeploymentId) {
        let file: RouterFile = serde_json::from_value(serde_json::json!({
            "nodes": [{"id": "a", "url": "http://192.0.2.10:11434"}],
            "routes": [{"name": "Coder", "deployments": [{"node": "a", "model": "QwenCoder"}]}]
        }))
        .unwrap();
        let topology = validate(file, &|_| None).unwrap().topology;
        let id = topology.deployments()[0].id.clone();
        (LoadBook::new(&topology), id)
    }

    #[test]
    fn a_lease_counts_while_held_and_gives_the_slot_back_on_drop() {
        let (book, id) = book();
        let first = book.acquire(&id);
        let second = book.acquire(&id);
        assert_eq!(book.active(&id), 2);
        drop(first);
        assert_eq!(book.active(&id), 1);
        drop(second);
        assert_eq!(book.active(&id), 0);
    }

    #[test]
    fn a_lease_dropped_by_a_panic_still_gives_the_slot_back() {
        let (book, id) = book();
        let book = Arc::new(book);
        let inside = Arc::clone(&book);
        let target = id.clone();
        let result = std::thread::spawn(move || {
            let _lease = inside.acquire(&target);
            panic!("the attempt blew up");
        })
        .join();
        assert!(result.is_err());
        assert_eq!(book.active(&id), 0);
    }

    #[test]
    fn concurrent_leases_balance_exactly() {
        let (book, id) = book();
        let book = Arc::new(book);
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let book = Arc::clone(&book);
                let id = id.clone();
                std::thread::spawn(move || {
                    for _ in 0..1000 {
                        let _lease = book.acquire(&id);
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(book.active(&id), 0);
    }
}
