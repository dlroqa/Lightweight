//! Identifiers that leave the machine, or that key a run's whole history.
//!
//! A [`RunId`] is the spine of a run: every event, every appended message and
//! every audit line carries it. Because a run must be able to *start* even when
//! the OS entropy source is momentarily unavailable, [`RunId::new`] is
//! infallible — it prefers 128 bits of `getrandom` entropy and falls back to a
//! process-local counter mixed with the wall clock, so two runs never collide
//! and a run is never refused for want of randomness.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Identifies one agent run, from the first event to the terminal one.
///
/// Rendered as `run-` followed by 32 lowercase hex characters, so it is safe in
/// a filename, a URL and a log line without escaping.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(String);

impl RunId {
    /// Mint a new run id. Infallible by construction.
    ///
    /// The happy path is 16 bytes of OS entropy. If that call ever fails — a
    /// sandbox with no `getrandom`, an exhausted pool — the id is still unique:
    /// a monotonic counter guarantees no two ids in this process match, and the
    /// nanosecond clock keeps ids from different processes apart in practice.
    /// The alternative, refusing to start the run, is the one outcome an agent
    /// harness cannot afford.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let mut bytes = [0u8; 16];
        if getrandom::fill(&mut bytes).is_ok() {
            return Self(format!("run-{}", hex(&bytes)));
        }
        Self(format!("run-{}", fallback()))
    }

    /// The id as it appears everywhere.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for RunId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// Render bytes as lowercase hex, no separators.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Two hex digits per byte; `char::from_digit` never fails for < 16.
        out.push(nibble(byte >> 4));
        out.push(nibble(byte & 0x0f));
    }
    out
}

fn nibble(value: u8) -> char {
    match value {
        0..=9 => (b'0' + value) as char,
        _ => (b'a' + (value - 10)) as char,
    }
}

/// A collision-resistant identifier without any OS entropy.
///
/// The counter is the guarantee within a process; the clock separates
/// processes. Together they render as 32 hex characters so the shape matches an
/// entropy-derived id exactly.
fn fallback() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0);
    format!("{nanos:016x}{sequence:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn run_id_is_prefixed_and_unique() {
        let a = RunId::new();
        let b = RunId::new();
        assert!(a.as_str().starts_with("run-"), "{}", a);
        // "run-" plus 32 hex characters.
        assert_eq!(a.as_str().len(), 4 + 32);
        assert!(a.as_str()[4..].chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);

        let ids: HashSet<_> = (0..1000).map(|_| RunId::new().0).collect();
        assert_eq!(ids.len(), 1000, "ids must not collide");
    }

    #[test]
    fn run_id_fallback_never_panics() {
        // The fallback path is what lets a run start when entropy is
        // unavailable. Exercise it directly and prove it stays unique.
        let ids: HashSet<_> = (0..1000).map(|_| fallback()).collect();
        assert_eq!(ids.len(), 1000);
        for id in &ids {
            assert_eq!(id.len(), 32);
            assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        }
    }
}
