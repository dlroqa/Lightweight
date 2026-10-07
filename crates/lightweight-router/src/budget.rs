//! The pre-commit request budget (R9.3.2): one client request's single,
//! absolute deadline for reaching response commit.
//!
//! It answers one question — *may this request still spend time trying to
//! reach a response head?* — and nothing else. It never chooses, scores,
//! filters or orders a route, a deployment or a fallback: no function that
//! decides one of those takes a budget.
//!
//! * **One deadline.** [`RequestBudget::start`] is called once per client
//!   request, in `proxy::forward_as`, at the instant every other router-side
//!   duration starts. The value is `Copy` and has no setter: every stage —
//!   classification, scoring, planning, each deployment attempt, each fallback
//!   route — derives what is left from the same instant. The router's own
//!   classification request inherits its parent's budget and never starts one.
//! * **A gate and a cap.** Before new work starts, [`expired`] refuses it.
//!   Every pre-commit wait is [`bound`] by the deadline, so an existing limit
//!   (the connect timeout, a classifier's own timeout) is effectively
//!   `min(own limit, remaining)` and an unbounded one (the wait for a response
//!   head) is the remainder.
//! * **Deterministic.** [`bound`] is `tokio::time::timeout_at`, whose `poll`
//!   polls the wrapped operation before the timer, every time: an operation
//!   that is ready when the deadline is due wins. No unbiased `select!` is
//!   ever used to race the two.
//! * **Pre-commit only.** Nothing holds or polls the budget once a response
//!   head is committed.
//!
//! Absent from the configuration, there is no budget: every check passes and
//! no timer is ever registered.

use std::future::Future;
use std::time::Duration;

use serde::Serialize;
use tokio::time::Instant;

/// The smallest budget a configuration may set, in milliseconds.
pub const MIN_BUDGET_MS: u64 = 1_000;
/// The largest budget a configuration may set, in milliseconds (one hour).
pub const MAX_BUDGET_MS: u64 = 3_600_000;

/// The error code of the terminal `504` a budget ends a request with.
pub const EXHAUSTED: &str = "request_budget_exhausted";

/// One client request's pre-commit budget. Carried as
/// `Option<RequestBudget>`: `None` is "not configured".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestBudget {
    /// `started + configured`. Set once; there is no setter.
    deadline: Instant,
    /// The operator's number, for the trace and the `504` message only.
    configured: Duration,
    /// For the trace's `elapsed_*` only.
    started: Instant,
}

impl RequestBudget {
    /// Start a client request's budget now. Called only by `forward_as`.
    pub(crate) fn start(configured: Duration) -> Self {
        let started = Instant::now();
        Self {
            deadline: started + configured,
            configured,
            started,
        }
    }

    /// The one absolute deadline every stage of this request shares.
    pub const fn deadline(&self) -> Instant {
        self.deadline
    }

    pub const fn configured(&self) -> Duration {
        self.configured
    }

    /// What is left. Never negative.
    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    /// Whether the deadline has been reached. Equality counts as expired, so
    /// `remaining == 0` never starts work.
    pub fn expired(&self) -> bool {
        Instant::now() >= self.deadline
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
}

/// Whether `budget` refuses new work now. No budget never refuses.
pub fn expired(budget: Option<RequestBudget>) -> bool {
    budget.is_some_and(|budget| budget.expired())
}

/// The deadline cut a pre-commit wait before its operation completed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cut;

/// Wait for `operation`, but no later than the budget's deadline.
///
/// With no budget the operation is awaited exactly as before, with no timer.
/// With one, it is `timeout_at(deadline, operation)`: the operation is polled
/// before the deadline on every poll, so a result that is ready when the
/// deadline is due is the result. Only an operation still pending when the
/// deadline is observed is dropped — which closes whatever connection it
/// held.
pub async fn bound<F: Future>(
    budget: Option<RequestBudget>,
    operation: F,
) -> Result<F::Output, Cut> {
    match budget {
        None => Ok(operation.await),
        Some(budget) => tokio::time::timeout_at(budget.deadline, operation)
            .await
            .map_err(|_| Cut),
    }
}

/// Where a request was when its budget ended it. Exactly four values; never a
/// route name, an id or a number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Stage {
    /// The classify wait was cut, the nested classifier answered
    /// `request_budget_exhausted`, or the start check before classification
    /// refused.
    Classifier,
    /// After classification (or explicit resolution), before the initial
    /// route's first deployment attempt was sent.
    RoutePlanning,
    /// The initial route had started: an attempt was cut, or the start check
    /// before its next deployment refused.
    SameRouteAttempt,
    /// The start check before a fallback route refused, or a fallback route's
    /// attempt was cut or its next deployment refused.
    CrossRouteFallback,
}

impl Stage {
    pub const ALL: [Self; 4] = [
        Self::Classifier,
        Self::RoutePlanning,
        Self::SameRouteAttempt,
        Self::CrossRouteFallback,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Classifier => "classifier",
            Self::RoutePlanning => "route_planning",
            Self::SameRouteAttempt => "same_route_attempt",
            Self::CrossRouteFallback => "cross_route_fallback",
        }
    }
}

/// A request's budget in its trace: timings and bounded values only.
///
/// Three shapes: committed (`elapsed_before_commit_ms`,
/// `remaining_at_commit_ms`), exhausted (`elapsed_ms`, `remaining_ms`,
/// `stage`, maybe `next_unattempted_route`), and ended some other way without
/// a commit (`elapsed_ms`, `remaining_ms`, `exhausted: false`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BudgetTrace {
    pub configured_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_before_commit_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining_at_commit_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining_ms: Option<u64>,
    /// `true` only when the request was answered `504
    /// request_budget_exhausted`.
    pub exhausted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<&'static str>,
    /// The logical route a start check refused — the initial route in
    /// `route_planning`, or the next fallback route at a transition. Never
    /// counted as attempted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_unattempted_route: Option<String>,
}

impl BudgetTrace {
    fn base(budget: &RequestBudget) -> Self {
        Self {
            configured_ms: ms(budget.configured),
            elapsed_before_commit_ms: None,
            remaining_at_commit_ms: None,
            elapsed_ms: None,
            remaining_ms: None,
            exhausted: false,
            stage: None,
            next_unattempted_route: None,
        }
    }

    /// The budget's state as a response head is committed.
    pub fn committed(budget: &RequestBudget) -> Self {
        Self {
            elapsed_before_commit_ms: Some(ms(budget.elapsed())),
            remaining_at_commit_ms: Some(ms(budget.remaining())),
            ..Self::base(budget)
        }
    }

    /// A request that ended without a commit, for a reason of its own.
    pub fn ended(budget: &RequestBudget) -> Self {
        Self {
            elapsed_ms: Some(ms(budget.elapsed())),
            remaining_ms: Some(ms(budget.remaining())),
            ..Self::base(budget)
        }
    }

    /// A request the budget ended.
    pub fn exhausted(budget: &RequestBudget, stage: Stage, next: Option<String>) -> Self {
        Self {
            exhausted: true,
            stage: Some(stage.as_str()),
            next_unattempted_route: next,
            ..Self::ended(budget)
        }
    }
}

fn ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Marks the router's own `504 request_budget_exhausted` response, so the
/// parent of a nested classification request can tell it from a node's `504`
/// without reading a body. A response extension: it never leaves the process.
#[derive(Clone, Copy, Debug)]
pub struct ExhaustedMarker;

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;

    const BUDGET: Duration = Duration::from_millis(5_000);

    #[tokio::test(start_paused = true)]
    async fn one_absolute_deadline_that_time_only_shrinks() {
        let budget = RequestBudget::start(BUDGET);
        let deadline = budget.deadline();
        assert_eq!(budget.remaining(), BUDGET);
        assert!(!budget.expired());

        tokio::time::advance(Duration::from_millis(2_000)).await;
        assert_eq!(budget.deadline(), deadline, "the deadline never moves");
        assert_eq!(budget.remaining(), Duration::from_millis(3_000));
        assert_eq!(budget.elapsed(), Duration::from_millis(2_000));

        // A copy is the same budget, not a fresh one.
        let copy = budget;
        assert_eq!(copy.deadline(), deadline);
        assert_eq!(copy.remaining(), Duration::from_millis(3_000));

        tokio::time::advance(Duration::from_millis(3_000)).await;
        assert_eq!(budget.remaining(), Duration::ZERO);
        assert!(budget.expired(), "remaining == 0 is expired");
        tokio::time::advance(Duration::from_millis(10)).await;
        assert_eq!(budget.remaining(), Duration::ZERO, "never negative");
    }

    #[tokio::test(start_paused = true)]
    async fn no_budget_never_refuses_and_never_cuts() {
        assert!(!expired(None));
        let slow = async {
            tokio::time::sleep(Duration::from_secs(7_200)).await;
            7
        };
        assert_eq!(bound(None, slow).await, Ok(7));
    }

    /// Run one race: the operation completes at `deadline + offset_ms`
    /// (negative is before), through a oneshot, under paused time.
    async fn race(offset_ms: i64) -> Result<&'static str, Cut> {
        let budget = RequestBudget::start(BUDGET);
        let (tx, rx) = oneshot::channel::<&'static str>();
        let at = if offset_ms >= 0 {
            budget.deadline() + Duration::from_millis(offset_ms.unsigned_abs())
        } else {
            budget.deadline() - Duration::from_millis(offset_ms.unsigned_abs())
        };
        tokio::spawn(async move {
            tokio::time::sleep_until(at).await;
            let _ = tx.send("operation");
        });
        bound(Some(budget), async move { rx.await.unwrap_or("dropped") }).await
    }

    #[tokio::test(start_paused = true)]
    async fn a_result_before_the_deadline_wins() {
        assert_eq!(race(-1).await, Ok("operation"));
    }

    #[tokio::test(start_paused = true)]
    async fn the_deadline_wins_over_an_operation_that_has_not_completed() {
        assert_eq!(race(1).await, Err(Cut));
    }

    /// The operation's result is sent before the clock reaches the deadline is
    /// observed, so both are ready in the same poll: the operation wins.
    #[tokio::test(start_paused = true)]
    async fn when_both_are_ready_in_one_poll_the_operation_wins() {
        let budget = RequestBudget::start(BUDGET);
        let (tx, rx) = oneshot::channel::<&'static str>();
        let _ = tx.send("operation");
        // The clock is already at the deadline when the wait is first polled.
        tokio::time::advance(BUDGET).await;
        assert!(budget.expired());
        assert_eq!(
            bound(Some(budget), async move { rx.await.unwrap_or("dropped") }).await,
            Ok("operation")
        );
    }

    /// B25: the same answer every time, 1 000 times each, for an operation
    /// completing just before, exactly at, and just after the deadline.
    #[tokio::test(start_paused = true)]
    async fn the_race_is_deterministic_over_a_thousand_runs() {
        for _ in 0..1_000 {
            assert_eq!(race(-1).await, Ok("operation"));
            assert_eq!(race(1).await, Err(Cut));
        }
        for _ in 0..1_000 {
            let budget = RequestBudget::start(BUDGET);
            let (tx, rx) = oneshot::channel::<&'static str>();
            let _ = tx.send("operation");
            tokio::time::advance(BUDGET).await;
            assert_eq!(
                bound(Some(budget), async move { rx.await.unwrap_or("dropped") }).await,
                Ok("operation")
            );
        }
    }

    #[test]
    fn the_trace_holds_timings_and_bounded_values_only() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap();
        rt.block_on(async {
            let budget = RequestBudget::start(Duration::from_millis(30_000));
            tokio::time::advance(Duration::from_millis(27_120)).await;
            let committed = serde_json::to_value(BudgetTrace::committed(&budget)).unwrap();
            assert_eq!(
                committed,
                serde_json::json!({"configured_ms": 30000, "elapsed_before_commit_ms": 27120,
                                   "remaining_at_commit_ms": 2880, "exhausted": false})
            );
            tokio::time::advance(Duration::from_millis(2_884)).await;
            let exhausted = serde_json::to_value(BudgetTrace::exhausted(
                &budget,
                Stage::CrossRouteFallback,
                Some("Reasoning".into()),
            ))
            .unwrap();
            assert_eq!(
                exhausted,
                serde_json::json!({"configured_ms": 30000, "elapsed_ms": 30004, "remaining_ms": 0,
                                   "exhausted": true, "stage": "cross_route_fallback",
                                   "next_unattempted_route": "Reasoning"})
            );
            let ended = serde_json::to_value(BudgetTrace::ended(&budget)).unwrap();
            assert_eq!(
                ended,
                serde_json::json!({"configured_ms": 30000, "elapsed_ms": 30004, "remaining_ms": 0,
                                   "exhausted": false})
            );
        });
    }

    /// Acceptance criteria 5 and 6 (B25, B32, B33, M12, M13, M18): nothing
    /// that decides a route, a deployment, a fallback order or a placement can
    /// read the budget — none of those modules names it — and the request path
    /// never races an operation against the deadline with an unbiased
    /// `select!`.
    #[test]
    fn no_deciding_module_reads_the_budget_and_no_unbiased_select_races_it() {
        let deciding = [
            ("scoring/mod.rs", include_str!("scoring/mod.rs")),
            ("scoring/history.rs", include_str!("scoring/history.rs")),
            ("select.rs", include_str!("select.rs")),
            ("capability.rs", include_str!("capability.rs")),
            ("affinity.rs", include_str!("affinity.rs")),
            ("load.rs", include_str!("load.rs")),
            ("fallback.rs", include_str!("fallback.rs")),
            ("placement.rs", include_str!("placement.rs")),
            ("controller.rs", include_str!("controller.rs")),
        ];
        for (name, source) in deciding {
            assert!(
                !source.to_lowercase().contains("budget"),
                "{name} must not read the request budget"
            );
        }
        let request_path = [
            ("proxy.rs", include_str!("proxy.rs")),
            ("budget.rs", include_str!("budget.rs")),
            ("classifier/mod.rs", include_str!("classifier/mod.rs")),
            (
                "classifier/lightweight.rs",
                include_str!("classifier/lightweight.rs"),
            ),
            ("classifier/jev.rs", include_str!("classifier/jev.rs")),
        ];
        let pattern = ["select", "!"].concat();
        for (name, source) in request_path {
            let code = source
                .lines()
                .filter(|line| !line.trim_start().starts_with("//"))
                .collect::<Vec<_>>();
            for (index, line) in code.iter().enumerate() {
                if line.contains(&pattern) {
                    let next = code.get(index + 1).copied().unwrap_or("");
                    assert!(
                        next.trim() == "biased;",
                        "{name}: a select on the request path must be `biased;`"
                    );
                }
            }
        }
    }

    #[test]
    fn the_stage_vocabulary_is_exactly_four_values() {
        let names: Vec<&str> = Stage::ALL.iter().map(|stage| stage.as_str()).collect();
        assert_eq!(
            names,
            [
                "classifier",
                "route_planning",
                "same_route_attempt",
                "cross_route_fallback"
            ]
        );
    }
}
