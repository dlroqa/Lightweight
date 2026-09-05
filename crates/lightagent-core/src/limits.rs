//! Resource limits for a run.
//!
//! A run is bounded on every axis a runaway model could push: how many turns it
//! may take, how many tools it may call, how long it may run in wall-clock
//! time, how many identical calls it may repeat, and how large a single tool's
//! output may be. The defaults are the master-plan numbers.
//!
//! [`RunLimits::intersect`] takes the more restrictive of two bounds on every
//! axis. That is what a delegated child run needs: it can be given tighter
//! limits than its parent, but never looser, so intersecting the two is always
//! safe.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Bounds enforced across a whole run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RunLimits {
    /// Maximum model turns (a turn is one model completion, tool round
    /// included). Master-plan `max_tool_rounds`.
    pub max_turns: u32,
    /// Maximum tool calls over the whole run.
    pub max_tool_calls: u32,
    /// Maximum tools that may run at once. One today; the seam for parallel
    /// workers later.
    pub max_parallel_tools: u32,
    /// How many times the same call may repeat before the run is stopped as
    /// looping.
    pub max_repeated_identical_calls: u32,
    /// Maximum bytes of a single tool's output kept in the transcript.
    pub max_tool_output_bytes: usize,
    /// Wall-clock budget in seconds. `None` means unbounded in time.
    pub wall_clock_secs: Option<u64>,
}

impl Default for RunLimits {
    fn default() -> Self {
        Self {
            max_turns: 8,
            max_tool_calls: 32,
            max_parallel_tools: 1,
            max_repeated_identical_calls: 2,
            max_tool_output_bytes: 262_144,
            wall_clock_secs: Some(300),
        }
    }
}

impl RunLimits {
    /// The wall-clock budget as a [`Duration`], if one is set.
    pub fn wall_clock(&self) -> Option<Duration> {
        self.wall_clock_secs.map(Duration::from_secs)
    }

    /// Combine two limits, taking the more restrictive bound on every axis.
    ///
    /// A `None` wall clock is "unbounded", so it always loses to a `Some`.
    pub fn intersect(&self, other: &RunLimits) -> RunLimits {
        RunLimits {
            max_turns: self.max_turns.min(other.max_turns),
            max_tool_calls: self.max_tool_calls.min(other.max_tool_calls),
            max_parallel_tools: self.max_parallel_tools.min(other.max_parallel_tools),
            max_repeated_identical_calls: self
                .max_repeated_identical_calls
                .min(other.max_repeated_identical_calls),
            max_tool_output_bytes: self.max_tool_output_bytes.min(other.max_tool_output_bytes),
            wall_clock_secs: min_opt(self.wall_clock_secs, other.wall_clock_secs),
        }
    }
}

/// The smaller of two optional bounds, treating `None` as unbounded.
fn min_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_master_plan() {
        let limits = RunLimits::default();
        assert_eq!(limits.max_turns, 8);
        assert_eq!(limits.max_parallel_tools, 1);
        assert_eq!(limits.max_repeated_identical_calls, 2);
        assert_eq!(limits.max_tool_output_bytes, 262_144);
    }

    #[test]
    fn intersect_takes_the_tighter_bound() {
        let parent = RunLimits {
            max_turns: 8,
            max_tool_calls: 32,
            wall_clock_secs: None,
            ..RunLimits::default()
        };
        let child = RunLimits {
            max_turns: 3,
            max_tool_calls: 64,
            wall_clock_secs: Some(30),
            ..RunLimits::default()
        };
        let merged = parent.intersect(&child);
        assert_eq!(merged.max_turns, 3, "the smaller turn budget wins");
        assert_eq!(merged.max_tool_calls, 32, "the smaller call budget wins");
        assert_eq!(
            merged.wall_clock_secs,
            Some(30),
            "a bounded clock beats an unbounded one"
        );
    }
}
