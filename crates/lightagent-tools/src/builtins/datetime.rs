//! `datetime.now` — the current time, read-only.
//!
//! The smallest useful tool and the one the loop is first proven against. It is
//! [`RiskClass::Observe`](lightagent_core::RiskClass::Observe), so the default
//! policy runs it unattended, and it reads the context's [`Clock`], so a test
//! pins the instant and asserts an exact rendering rather than a moving one.

use async_trait::async_trait;
use lightagent_core::{RiskClass, ToolOutcome};
use serde_json::{Value, json};

use crate::context::ToolCtx;
use crate::definition::{Tool, ToolDefinition};

/// The `datetime.now` tool.
pub struct DateTimeNow {
    definition: ToolDefinition,
}

impl DateTimeNow {
    /// The tool's stable name.
    pub const NAME: &'static str = "datetime.now";

    /// Build the tool with its declaration.
    pub fn new() -> Self {
        let parameters = json!({
            "type": "object",
            "properties": {
                "tz": {
                    "type": "string",
                    "enum": ["utc"],
                    "description": "Timezone for the returned time; only \"utc\" is supported.",
                }
            },
            "additionalProperties": false,
        });
        Self {
            definition: ToolDefinition::new(
                Self::NAME,
                "The current date and time in UTC (RFC 3339).",
                parameters,
                RiskClass::Observe,
                Vec::new(),
            ),
        }
    }
}

impl Default for DateTimeNow {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for DateTimeNow {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn call(&self, _args: &Value, ctx: &ToolCtx) -> ToolOutcome {
        ToolOutcome::ok(rfc3339_utc(ctx.clock.now()))
    }
}

/// Render a [`SystemTime`](std::time::SystemTime) as RFC-3339 UTC, without a
/// date-time dependency. Sub-second precision is dropped: whole seconds are what
/// a tool result needs.
fn rfc3339_utc(time: std::time::SystemTime) -> String {
    let secs = time
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hour, minute, second) = (rem / 3_600, (rem % 3_600) / 60, rem % 60);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Convert a count of days since 1970-01-01 to a civil (year, month, day).
///
/// Howard Hinnant's `civil_from_days`, valid across the whole proleptic
/// Gregorian range and exact in integer arithmetic — no floating point, no
/// lookup tables, no leap-year special-casing beyond the era algebra.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // day of era, [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::Clock;
    use std::time::{Duration, UNIX_EPOCH};
    use tokio_util::sync::CancellationToken;

    #[test]
    fn epoch_renders_as_the_unix_epoch() {
        assert_eq!(rfc3339_utc(UNIX_EPOCH), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn a_known_instant_renders_exactly() {
        // 1_700_000_000 = 2023-11-14T22:13:20Z (verified independently).
        let time = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(rfc3339_utc(time), "2023-11-14T22:13:20Z");
    }

    #[tokio::test]
    async fn call_reads_the_injected_clock() {
        let fixed = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let ctx = ToolCtx::new(CancellationToken::new()).with_clock(Clock::Fixed(fixed));
        let outcome = DateTimeNow::new().call(&json!({}), &ctx).await;
        assert!(!outcome.is_error);
        assert_eq!(outcome.content, "2023-11-14T22:13:20Z");
    }
}
