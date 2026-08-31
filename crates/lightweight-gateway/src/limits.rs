//! Per-key request limits.
//!
//! A key handed to another machine's agent is a tap that, left open, can run
//! this gateway's single CPU engine flat out. A limit per key is how the owner
//! caps one consumer without touching the others: a rolling per-minute ceiling
//! for burst, an optional per-day ceiling for total volume.
//!
//! What is metered, and what is not:
//!
//! * **Only named keys.** The panel polls this gateway every second and the
//!   desktop shell drives it constantly; both are loopback and anonymous, and
//!   metering them against a user's own budget would be absurd. The in-flight
//!   gauge already excludes the panel's own polling for the same reason. A
//!   request that authenticated with a named key is the only thing counted.
//! * **In memory only.** A limit is a live guard, not a record; it resets when
//!   the gateway restarts, and nothing here is written to disk. The counters do
//!   double as the "last used" and total the panel shows per key, so that
//!   display costs no extra bookkeeping.
//!
//! The window is a timestamp deque rather than a fixed bucket: a fixed
//! minute-aligned bucket lets a caller send its whole minute's allowance at
//! :59 and again at :00, twice the intended rate across the boundary. A rolling
//! sixty seconds cannot be gamed that way.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use lightweight_store::RateLimit;

/// A rolling window is sixty seconds wide.
const MINUTE: Duration = Duration::from_secs(60);
/// A day, for the per-day ceiling's reset.
const DAY_SECS: u64 = 86_400;

/// One key's usage, tracked live.
#[derive(Default)]
struct Usage {
    /// When each recent request landed, pruned to the last minute on each look.
    recent: VecDeque<Instant>,
    /// Requests counted against the current UTC day.
    day_count: u32,
    /// The UTC day number `day_count` belongs to; a change resets it.
    day: u64,
    /// Total requests since this gateway started, for display.
    total: u64,
    /// When the key was last seen, unix seconds, for display.
    last_used: u64,
}

/// Why a request was refused, and how long until it would be admitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Throttled {
    /// Seconds to wait before retrying, for the `Retry-After` header.
    pub retry_after: u64,
    /// Which ceiling was hit, for the message.
    pub which: Ceiling,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ceiling {
    PerMinute,
    PerDay,
}

/// A read-only view of one key's usage, for the control surface.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsageSnapshot {
    pub total: u64,
    pub last_used: Option<u64>,
    pub in_last_minute: u32,
    pub today: u32,
}

/// Every key's live usage.
#[derive(Default)]
pub struct RateLimiter {
    keys: Mutex<HashMap<String, Usage>>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Count a request against `key_id`, or refuse it if a ceiling is reached.
    ///
    /// A refusal does *not* count: a caller that is over its limit and keeps
    /// trying must not push its own next-allowed time forever outward. Only an
    /// admitted request is recorded.
    pub fn admit(&self, key_id: &str, limit: RateLimit) -> Result<(), Throttled> {
        let now = Instant::now();
        let today = unix_now() / DAY_SECS;
        let mut keys = self
            .keys
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let usage = keys.entry(key_id.to_owned()).or_default();

        // Prune the rolling window before either decision reads it.
        while let Some(&front) = usage.recent.front() {
            if now.duration_since(front) >= MINUTE {
                usage.recent.pop_front();
            } else {
                break;
            }
        }
        if usage.day != today {
            usage.day = today;
            usage.day_count = 0;
        }

        if let Some(per_minute) = limit.per_minute
            && usage.recent.len() as u32 >= per_minute
        {
            let oldest = usage.recent.front().copied().unwrap_or(now);
            let wait = MINUTE.saturating_sub(now.duration_since(oldest));
            return Err(Throttled {
                retry_after: wait.as_secs().max(1),
                which: Ceiling::PerMinute,
            });
        }
        if let Some(per_day) = limit.per_day
            && usage.day_count >= per_day
        {
            let until_midnight = DAY_SECS - (unix_now() % DAY_SECS);
            return Err(Throttled {
                retry_after: until_midnight.max(1),
                which: Ceiling::PerDay,
            });
        }

        usage.recent.push_back(now);
        usage.day_count += 1;
        usage.total += 1;
        usage.last_used = unix_now();
        Ok(())
    }

    /// What one key has done, for the panel.
    pub fn snapshot(&self, key_id: &str) -> UsageSnapshot {
        let now = Instant::now();
        let keys = self
            .keys
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let Some(usage) = keys.get(key_id) else {
            return UsageSnapshot::default();
        };
        let in_last_minute = usage
            .recent
            .iter()
            .filter(|&&at| now.duration_since(at) < MINUTE)
            .count() as u32;
        UsageSnapshot {
            total: usage.total,
            last_used: (usage.last_used != 0).then_some(usage.last_used),
            in_last_minute,
            today: usage.day_count,
        }
    }

    /// Drop a revoked key's counters, so a reissued id does not inherit them.
    pub fn forget(&self, key_id: &str) {
        self.keys
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(key_id);
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_per_minute_ceiling_refuses_the_next_request() {
        let limiter = RateLimiter::new();
        let limit = RateLimit {
            per_minute: Some(2),
            per_day: None,
        };
        assert!(limiter.admit("k", limit).is_ok());
        assert!(limiter.admit("k", limit).is_ok());
        let refused = limiter.admit("k", limit).expect_err("third is refused");
        assert_eq!(refused.which, Ceiling::PerMinute);
        assert!(refused.retry_after >= 1);
    }

    #[test]
    fn an_unlimited_key_is_never_refused() {
        let limiter = RateLimiter::new();
        for _ in 0..1000 {
            assert!(limiter.admit("k", RateLimit::default()).is_ok());
        }
    }

    #[test]
    fn a_refused_request_is_not_counted() {
        let limiter = RateLimiter::new();
        let limit = RateLimit {
            per_minute: Some(1),
            per_day: None,
        };
        assert!(limiter.admit("k", limit).is_ok());
        assert!(limiter.admit("k", limit).is_err());
        // The snapshot shows one admitted request, not two attempts.
        assert_eq!(limiter.snapshot("k").in_last_minute, 1);
        assert_eq!(limiter.snapshot("k").total, 1);
    }

    #[test]
    fn a_per_day_ceiling_is_enforced_independently() {
        let limiter = RateLimiter::new();
        let limit = RateLimit {
            per_minute: None,
            per_day: Some(1),
        };
        assert!(limiter.admit("k", limit).is_ok());
        let refused = limiter.admit("k", limit).expect_err("over the day");
        assert_eq!(refused.which, Ceiling::PerDay);
    }

    #[test]
    fn keys_are_metered_separately() {
        let limiter = RateLimiter::new();
        let limit = RateLimit {
            per_minute: Some(1),
            per_day: None,
        };
        assert!(limiter.admit("a", limit).is_ok());
        assert!(limiter.admit("b", limit).is_ok(), "b has its own budget");
        assert!(limiter.admit("a", limit).is_err());
    }

    #[test]
    fn forgetting_a_key_clears_its_counters() {
        let limiter = RateLimiter::new();
        let limit = RateLimit {
            per_minute: Some(1),
            per_day: None,
        };
        assert!(limiter.admit("k", limit).is_ok());
        limiter.forget("k");
        assert!(
            limiter.admit("k", limit).is_ok(),
            "a forgotten key starts fresh"
        );
    }
}
