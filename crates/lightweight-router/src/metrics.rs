//! The router's own numbers, in Prometheus text.
//!
//! The gateway already serves `/metrics`, so the router does too, in the same
//! format, under names of its own. Labels are only ever configured names — a
//! route, a node — never anything a client typed: a request for a route that
//! does not exist is counted under `_unknown`, so a client cannot grow the
//! label set by inventing model names.
//!
//! The same discipline holds for everything added for observability: labels
//! are a route, a route's policy, a configured deployment, or a reason from a
//! fixed list. Never a session, a request id, a prompt, a tool name or an
//! address.
//!
//! Every duration and ratio here is **measured, never consulted**: no policy
//! reads a histogram. They exist so an operator can tell a slow router from a
//! slow model, and so a later release can change a heuristic on evidence.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crate::affinity::AffinityBook;
use crate::domain::{DeploymentId, NodeHealth, NodeId};
use crate::health::NodeStatus;

/// The label a request for an unconfigured route is counted under.
pub const UNKNOWN_ROUTE: &str = "_unknown";

/// How one request ended, as far as the router is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Outcome {
    Ok,
    ClientError,
    ServerError,
    /// No deployment could take it. Separate from `server_error` because it
    /// is the one number that says capacity, not correctness, ran out.
    Unavailable,
}

impl Outcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::ClientError => "client_error",
            Self::ServerError => "server_error",
            Self::Unavailable => "unavailable",
        }
    }

    pub fn of_status(status: u16) -> Self {
        match status {
            200..=399 => Self::Ok,
            400..=499 => Self::ClientError,
            _ => Self::ServerError,
        }
    }
}

#[derive(Debug, Default)]
pub struct RouterMetrics {
    requests: Mutex<BTreeMap<(String, Outcome), u64>>,
    failovers: Mutex<BTreeMap<String, u64>>,
    /// Committed routing decisions by route, policy and reason. Failovers
    /// show up here under their own reasons (`*_failover`), so decisions and
    /// failovers by policy are both one query away.
    decisions: Mutex<BTreeMap<(String, &'static str, &'static str), u64>>,
    /// Available deployments a request's requirements ruled out, by route and
    /// gap. One deployment failing two requirements counts under both.
    capability_filtered: Mutex<BTreeMap<(String, &'static str), u64>>,
    /// Requests refused with `route_capability_mismatch`, by route.
    capability_mismatches: Mutex<BTreeMap<String, u64>>,
    /// Failovers taken because a deployment refused the prompt as longer than
    /// its context, by route. Also counted in `failovers`.
    context_overflow_failovers: Mutex<BTreeMap<String, u64>>,
    active: Arc<AtomicU64>,
    /// Requests whose session's sticky deployment was still a candidate and
    /// went first, by route.
    affinity_hits: Mutex<BTreeMap<String, u64>>,
    /// Requests naming a session with no usable affinity — none yet, expired,
    /// or its deployment no longer valid — by route.
    affinity_misses: Mutex<BTreeMap<String, u64>>,
    /// Sessions moved to another deployment, by route and reason.
    affinity_reassignments: Mutex<BTreeMap<(String, &'static str), u64>>,
    /// Placement actions finished, by route, action and result.
    placement_actions: Mutex<BTreeMap<(String, &'static str, &'static str), u64>>,
    /// Placement actions that failed, by route and reason.
    placement_failures: Mutex<BTreeMap<(String, &'static str), u64>>,
    /// Routes `Auto` chose, by rule (a configured name, or `_fallback`) and
    /// route. Counted when the route is chosen, whatever the route answers.
    auto_decisions: Mutex<BTreeMap<(String, String), u64>>,
    /// `Auto` requests no rule matched, by the fallback route they went to.
    auto_fallbacks: Mutex<BTreeMap<String, u64>>,
    /// Classifications, by provider and outcome.
    classifier_outcomes: Mutex<BTreeMap<(&'static str, &'static str), u64>>,
    /// Routes classifications chose (outcome `chosen`), by provider and route.
    classifier_routes: Mutex<BTreeMap<(&'static str, String), u64>>,
    /// Scored classifications (R9.2), by the route that won and whether it
    /// overrode the route R9.1 alone would have taken.
    scoring_decisions: Mutex<BTreeMap<(String, bool), u64>>,
    /// Classifications scoring left as R9.1 resolved them, by reason
    /// (`below_threshold`, `no_verdict`, `internal_error`).
    scoring_fallbacks: Mutex<BTreeMap<&'static str, u64>>,
    /// Finished requests counted toward route history, by route and
    /// observation (`success`, `failure`, `unavailable`, `mismatch`,
    /// `neutral`). Only while scoring is on.
    route_history_observations: Mutex<BTreeMap<(String, &'static str), u64>>,
    histograms: Histograms,
}

/// A histogram's bucket ladder: upper bounds in the integer unit values are
/// observed in, and how many of that unit make one of the exposed unit.
///
/// Integers so a bucket is chosen by comparison, with no float rounding
/// deciding which side of a boundary a value lands on (the gateway's rule).
#[derive(Clone, Copy, Debug)]
pub struct Ladder {
    pub bounds: &'static [i64],
    pub per_unit: i64,
}

/// Request-scale durations, observed in milliseconds and exposed in seconds.
/// From 5 ms to five minutes: a CPU prefill of a long prompt genuinely reaches
/// minutes, and a ladder that stopped at ten seconds would measure nothing
/// about it.
pub const SECONDS: Ladder = Ladder {
    bounds: &[
        5, 10, 25, 50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000, 30_000, 60_000, 120_000, 300_000,
    ],
    per_unit: 1_000,
};

/// The router's own planning time, observed in microseconds and exposed in
/// seconds. Planning reads memory and takes a lock or two; a millisecond
/// ladder would put every observation in its first bucket.
pub const PLANNING: Ladder = Ladder {
    bounds: &[10, 25, 50, 100, 250, 500, 1_000, 2_500, 10_000, 50_000],
    per_unit: 1_000_000,
};

/// `actual / estimated` prompt tokens, observed in thousandths. Above 1 means
/// the router's lower bound underestimated, which it is built to do. The top
/// reaches 32: on a real node a one-line prompt measured 4 estimated against
/// 36 counted, because the template's own markup dominates a short prompt.
pub const RATIO: Ladder = Ladder {
    bounds: &[
        250, 500, 750, 1_000, 1_250, 1_500, 2_000, 2_500, 3_000, 4_000, 5_000, 6_000, 8_000,
        10_000, 16_000, 32_000,
    ],
    per_unit: 1_000,
};

/// `actual − estimated` prompt tokens. Signed: a negative value is an
/// overestimate, which would mean the lower bound is not one.
pub const TOKENS: Ladder = Ladder {
    bounds: &[
        -4_096, -1_024, -256, -64, -16, 0, 16, 64, 256, 1_024, 4_096, 16_384, 65_536,
    ],
    per_unit: 1,
};

/// One labelled histogram series.
#[derive(Debug)]
struct Series {
    /// Per bucket, not cumulative; the `+Inf` overflow is the last.
    buckets: Vec<AtomicU64>,
    count: AtomicU64,
    sum: AtomicI64,
}

impl Series {
    fn new(ladder: Ladder) -> Self {
        Self {
            buckets: (0..=ladder.bounds.len())
                .map(|_| AtomicU64::new(0))
                .collect(),
            count: AtomicU64::new(0),
            sum: AtomicI64::new(0),
        }
    }

    fn observe(&self, ladder: Ladder, value: i64) {
        let index = ladder.bounds.partition_point(|bound| *bound < value);
        if let Some(bucket) = self.buckets.get(index) {
            bucket.fetch_add(1, Ordering::Relaxed);
        }
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum.fetch_add(value, Ordering::Relaxed);
    }
}

type Labels = Vec<(&'static str, String)>;

/// One histogram family: a name, its ladder, and a series per label set.
#[derive(Debug)]
struct Family {
    name: &'static str,
    help: &'static str,
    ladder: Ladder,
    series: Mutex<BTreeMap<Labels, Series>>,
}

impl Family {
    const fn new(name: &'static str, help: &'static str, ladder: Ladder) -> Self {
        Self {
            name,
            help,
            ladder,
            series: Mutex::new(BTreeMap::new()),
        }
    }

    fn observe(&self, labels: Labels, value: i64) {
        self.series
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(labels)
            .or_insert_with(|| Series::new(self.ladder))
            .observe(self.ladder, value);
    }

    /// How many observations a series has, for tests and the admin view.
    fn count(&self, labels: &Labels) -> u64 {
        self.series
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(labels)
            .map_or(0, |series| series.count.load(Ordering::Relaxed))
    }

    fn sum(&self, labels: &Labels) -> i64 {
        self.series
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(labels)
            .map_or(0, |series| series.sum.load(Ordering::Relaxed))
    }

    fn render(&self, out: &mut String) {
        let _ = writeln!(out, "# HELP {} {}", self.name, self.help);
        let _ = writeln!(out, "# TYPE {} histogram", self.name);
        let per_unit = self.ladder.per_unit as f64;
        for (labels, series) in self
            .series
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            let base: String = labels
                .iter()
                .map(|(key, value)| format!("{key}=\"{}\",", escape(value)))
                .collect();
            let mut running = 0;
            for (index, bucket) in series.buckets.iter().enumerate() {
                running += bucket.load(Ordering::Relaxed);
                let le = self
                    .ladder
                    .bounds
                    .get(index)
                    .map_or_else(|| "+Inf".to_owned(), |b| format_unit(*b as f64 / per_unit));
                let _ = writeln!(out, "{}_bucket{{{base}le=\"{le}\"}} {running}", self.name);
            }
            let trimmed = base.trim_end_matches(',');
            let braces = if trimmed.is_empty() {
                String::new()
            } else {
                format!("{{{trimmed}}}")
            };
            let _ = writeln!(
                out,
                "{}_sum{braces} {}",
                self.name,
                format_unit(series.sum.load(Ordering::Relaxed) as f64 / per_unit)
            );
            let _ = writeln!(
                out,
                "{}_count{braces} {}",
                self.name,
                series.count.load(Ordering::Relaxed)
            );
        }
    }
}

/// A number in the exposition format, without trailing zeros.
fn format_unit(value: f64) -> String {
    let text = format!("{value:.6}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    if text.is_empty() || text == "-" {
        "0".to_owned()
    } else {
        text.to_owned()
    }
}

/// Every histogram the router keeps.
#[derive(Debug)]
struct Histograms {
    request: Family,
    planning: Family,
    ttft: Family,
    upstream_ttft: Family,
    upstream_response: Family,
    upstream_duration: Family,
    estimation_ratio: Family,
    estimation_error: Family,
    reconcile: Family,
    classifier: Family,
}

impl Default for Histograms {
    fn default() -> Self {
        Self {
            request: Family::new(
                "router_request_duration_seconds",
                "From the router having the request to the end of its response, by route and policy. Includes failed and cancelled requests.",
                SECONDS,
            ),
            planning: Family::new(
                "router_routing_duration_seconds",
                "Time the router spent deciding where a request goes (parse, requirements, plan), by route and policy. Excludes every upstream wait.",
                PLANNING,
            ),
            ttft: Family::new(
                "router_ttft_seconds",
                "Streaming only: from the router having the request to the first content, reasoning or tool-call delta relayed to the client, by route and policy.",
                SECONDS,
            ),
            upstream_ttft: Family::new(
                "router_upstream_ttft_seconds",
                "Streaming only: from sending to the committed deployment to its first content, reasoning or tool-call delta, by route and deployment.",
                SECONDS,
            ),
            upstream_response: Family::new(
                "router_upstream_response_seconds",
                "From sending one attempt to a deployment to its response head, for every attempt that got one, by route and deployment.",
                SECONDS,
            ),
            upstream_duration: Family::new(
                "router_upstream_duration_seconds",
                "From sending to the committed deployment to the end of its response body (or the client leaving), by route and deployment.",
                SECONDS,
            ),
            estimation_ratio: Family::new(
                "router_context_estimation_ratio",
                "Node-counted prompt tokens divided by the router's lower-bound estimate, when the node reported usage and the estimate was positive. Above 1 is an underestimate.",
                RATIO,
            ),
            estimation_error: Family::new(
                "router_context_estimation_error_tokens",
                "Node-counted prompt tokens minus the router's lower-bound estimate, when the node reported usage.",
                TOKENS,
            ),
            reconcile: Family::new(
                "router_placement_reconcile_duration_seconds",
                "One placement pass: reading health, comparing each route with its target, and starting loads. Loads themselves run afterwards and are not included.",
                PLANNING,
            ),
            classifier: Family::new(
                "router_classifier_duration_seconds",
                "One Auto classification, from asking the classifier to its verdict, timeout or failure, by provider and outcome. Not part of router_routing_duration_seconds.",
                SECONDS,
            ),
        }
    }
}

fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

fn micros(duration: Duration) -> i64 {
    i64::try_from(duration.as_micros()).unwrap_or(i64::MAX)
}

/// Counts one request as active until dropped — which, for a stream, is when
/// the body ends or the client goes away.
#[derive(Debug)]
pub struct ActiveGuard(Arc<AtomicU64>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl RouterMetrics {
    pub fn record_request(&self, route: &str, outcome: Outcome) {
        *self
            .requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry((route.to_owned(), outcome))
            .or_default() += 1;
    }

    pub fn record_decision(&self, route: &str, policy: &'static str, reason: &'static str) {
        *self
            .decisions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry((route.to_owned(), policy, reason))
            .or_default() += 1;
    }

    pub fn decisions(&self, route: &str, reason: &str) -> u64 {
        self.decisions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|((r, _, why), _)| r == route && *why == reason)
            .map(|(_, count)| count)
            .sum()
    }

    pub fn record_capability_filtered(&self, route: &str, gap: &'static str) {
        *self
            .capability_filtered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry((route.to_owned(), gap))
            .or_default() += 1;
    }

    pub fn capability_filtered(&self, route: &str, gap: &str) -> u64 {
        self.capability_filtered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(route.to_owned(), gap))
            .copied()
            .unwrap_or_default()
    }

    pub fn record_capability_mismatch(&self, route: &str) {
        *self
            .capability_mismatches
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(route.to_owned())
            .or_default() += 1;
    }

    pub fn capability_mismatches(&self, route: &str) -> u64 {
        self.capability_mismatches
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(route)
            .copied()
            .unwrap_or_default()
    }

    pub fn record_context_overflow_failover(&self, route: &str) {
        *self
            .context_overflow_failovers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(route.to_owned())
            .or_default() += 1;
    }

    pub fn context_overflow_failovers(&self, route: &str) -> u64 {
        self.context_overflow_failovers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(route)
            .copied()
            .unwrap_or_default()
    }

    pub fn record_failover(&self, route: &str) {
        *self
            .failovers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(route.to_owned())
            .or_default() += 1;
    }

    pub fn enter(&self) -> ActiveGuard {
        self.active.fetch_add(1, Ordering::Relaxed);
        ActiveGuard(Arc::clone(&self.active))
    }

    pub fn active(&self) -> u64 {
        self.active.load(Ordering::Relaxed)
    }

    pub fn requests(&self, route: &str, outcome: Outcome) -> u64 {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(route.to_owned(), outcome))
            .copied()
            .unwrap_or_default()
    }

    pub fn failovers(&self, route: &str) -> u64 {
        self.failovers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(route)
            .copied()
            .unwrap_or_default()
    }

    pub fn record_affinity_hit(&self, route: &str) {
        bump(&self.affinity_hits, route.to_owned());
    }

    pub fn record_affinity_miss(&self, route: &str) {
        bump(&self.affinity_misses, route.to_owned());
    }

    pub fn record_affinity_reassignment(&self, route: &str, reason: &'static str) {
        bump(&self.affinity_reassignments, (route.to_owned(), reason));
    }

    pub fn record_placement_action(&self, route: &str, action: &'static str, result: &'static str) {
        bump(&self.placement_actions, (route.to_owned(), action, result));
    }

    pub fn record_auto_decision(&self, rule: &str, route: &str, fallback: bool) {
        bump(&self.auto_decisions, (rule.to_owned(), route.to_owned()));
        if fallback {
            bump(&self.auto_fallbacks, route.to_owned());
        }
    }

    /// How many times `rule` chose a route, whichever.
    pub fn auto_decisions(&self, rule: &str) -> u64 {
        self.auto_decisions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|((r, _), _)| r == rule)
            .map(|(_, count)| count)
            .sum()
    }

    pub fn record_classification(
        &self,
        provider: &'static str,
        outcome: &'static str,
        chosen: Option<&str>,
        elapsed: Duration,
    ) {
        bump(&self.classifier_outcomes, (provider, outcome));
        if let Some(route) = chosen {
            bump(&self.classifier_routes, (provider, route.to_owned()));
        }
        self.histograms.classifier.observe(
            vec![
                ("provider", provider.to_owned()),
                ("outcome", outcome.to_owned()),
            ],
            millis(elapsed),
        );
    }

    /// Classifications so far by `provider`, by outcome.
    pub fn classifier_outcomes(&self, provider: &str) -> BTreeMap<&'static str, u64> {
        self.classifier_outcomes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|((p, _), _)| *p == provider)
            .map(|((_, outcome), count)| (*outcome, *count))
            .collect()
    }

    /// One scoring resolution: a contest's winner, or the reason R9.1's route
    /// stood without one.
    pub fn record_scoring(
        &self,
        reason: crate::scoring::ScoringReason,
        winner: &str,
        overrode: bool,
    ) {
        if reason.contested() {
            bump(&self.scoring_decisions, (winner.to_owned(), overrode));
        } else {
            bump(&self.scoring_fallbacks, reason.as_str());
        }
    }

    pub fn scoring_decisions(&self, route: &str, overrode: bool) -> u64 {
        read(&self.scoring_decisions, &(route.to_owned(), overrode))
    }

    pub fn scoring_fallbacks(&self, reason: &'static str) -> u64 {
        read(&self.scoring_fallbacks, &reason)
    }

    pub fn record_route_history(&self, route: &str, observation: &'static str) {
        bump(
            &self.route_history_observations,
            (route.to_owned(), observation),
        );
    }

    pub fn route_history_observations(&self, route: &str, observation: &'static str) -> u64 {
        read(
            &self.route_history_observations,
            &(route.to_owned(), observation),
        )
    }

    /// Each route's decayed history, read at scrape time. Nothing while
    /// scoring is off.
    pub fn route_history_to_prometheus(
        book: &crate::scoring::HistoryBook,
        now: std::time::Instant,
    ) -> String {
        let mut out = String::new();
        if !book.is_recording() {
            return out;
        }
        let view = book.view(now);
        out.push_str(
            "# HELP router_route_history_effective_samples Decayed scored observations (successes plus failures) per logical route.\n",
        );
        out.push_str("# TYPE router_route_history_effective_samples gauge\n");
        for route in &view {
            let _ = writeln!(
                out,
                "router_route_history_effective_samples{{route=\"{}\"}} {}",
                escape(&route.route),
                route.signal.effective_samples
            );
        }
        out.push_str(
            "# HELP router_route_history_signal The history term adaptive scoring reads per logical route, in (-1, 1); 0 below min_samples.\n",
        );
        out.push_str("# TYPE router_route_history_signal gauge\n");
        for route in &view {
            let _ = writeln!(
                out,
                "router_route_history_signal{{route=\"{}\"}} {}",
                escape(&route.route),
                route.signal.value
            );
        }
        out
    }

    pub fn auto_fallbacks(&self) -> u64 {
        self.auto_fallbacks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .sum()
    }

    pub fn record_placement_failure(&self, route: &str, reason: &'static str) {
        bump(&self.placement_failures, (route.to_owned(), reason));
    }

    pub fn placement_actions(
        &self,
        route: &str,
        action: &'static str,
        result: &'static str,
    ) -> u64 {
        read(&self.placement_actions, &(route.to_owned(), action, result))
    }

    pub fn placement_failures(&self, route: &str, reason: &'static str) -> u64 {
        read(&self.placement_failures, &(route.to_owned(), reason))
    }

    pub fn observe_reconcile(&self, elapsed: Duration) {
        self.histograms
            .reconcile
            .observe(Vec::new(), micros(elapsed));
    }

    pub fn reconcile_passes(&self) -> u64 {
        self.histograms.reconcile.count(&Vec::new())
    }

    /// Each route's placement target and ready deployments, read at scrape
    /// time from what the router has observed. Routes without a target are
    /// not listed.
    pub fn placement_to_prometheus(
        topology: &crate::domain::Topology,
        health: &BTreeMap<NodeId, NodeStatus>,
        loading: &std::collections::BTreeSet<DeploymentId>,
    ) -> String {
        let mut out = String::new();
        let assessments: Vec<_> = topology
            .routes()
            .iter()
            .filter_map(|route| crate::placement::assess(topology, route, health, loading))
            .collect();
        if assessments.is_empty() {
            return out;
        }
        for (name, help, value) in [
            (
                "router_placement_ready_deployments",
                "Deployments of the route the router observes ready, placed by the controller or not.",
                (|a: &crate::placement::Assessment| a.ready())
                    as fn(&crate::placement::Assessment) -> u32,
            ),
            (
                "router_placement_target_deployments",
                "The route's placement target: min_ready plus warm_standby.",
                |a| a.target(),
            ),
            (
                "router_placement_loading_deployments",
                "Loads the controller has in progress for the route.",
                |a| a.loading(),
            ),
        ] {
            let _ = writeln!(out, "# HELP {name} {help}");
            let _ = writeln!(out, "# TYPE {name} gauge");
            for assessment in &assessments {
                let _ = writeln!(
                    out,
                    "{name}{{route=\"{}\"}} {}",
                    escape(assessment.route.as_str()),
                    value(assessment)
                );
            }
        }
        out
    }

    pub fn affinity_hits(&self, route: &str) -> u64 {
        read(&self.affinity_hits, &route.to_owned())
    }

    pub fn affinity_misses(&self, route: &str) -> u64 {
        read(&self.affinity_misses, &route.to_owned())
    }

    pub fn affinity_reassignments(&self, route: &str, reason: &'static str) -> u64 {
        read(&self.affinity_reassignments, &(route.to_owned(), reason))
    }

    pub fn observe_request(&self, route: &str, policy: &'static str, elapsed: Duration) {
        self.histograms
            .request
            .observe(route_policy(route, policy), millis(elapsed));
    }

    pub fn observe_planning(&self, route: &str, policy: &'static str, elapsed: Duration) {
        self.histograms
            .planning
            .observe(route_policy(route, policy), micros(elapsed));
    }

    pub fn observe_ttft(&self, route: &str, policy: &'static str, elapsed: Duration) {
        self.histograms
            .ttft
            .observe(route_policy(route, policy), millis(elapsed));
    }

    pub fn observe_upstream_ttft(&self, route: &str, deployment: &str, elapsed: Duration) {
        self.histograms
            .upstream_ttft
            .observe(route_deployment(route, deployment), millis(elapsed));
    }

    pub fn observe_upstream_response(&self, route: &str, deployment: &str, elapsed: Duration) {
        self.histograms
            .upstream_response
            .observe(route_deployment(route, deployment), millis(elapsed));
    }

    pub fn observe_upstream_duration(&self, route: &str, deployment: &str, elapsed: Duration) {
        self.histograms
            .upstream_duration
            .observe(route_deployment(route, deployment), millis(elapsed));
    }

    /// Compare the router's prompt estimate with the node's own count.
    ///
    /// The error is recorded whenever the node reported a count. The ratio is
    /// `actual / estimate`, recorded only when the estimate is positive: a
    /// ratio over zero is not a number, and inventing one would put an
    /// infinity in a histogram.
    pub fn observe_estimate(&self, route: &str, estimated: u32, actual: u32) {
        let labels = vec![("route", route.to_owned())];
        self.histograms
            .estimation_error
            .observe(labels.clone(), i64::from(actual) - i64::from(estimated));
        if estimated > 0 {
            let thousandths = i64::from(actual) * 1_000 / i64::from(estimated);
            self.histograms
                .estimation_ratio
                .observe(labels, thousandths);
        }
    }

    /// How many observations a histogram has for one label set — for tests.
    /// `family` is the exposed name; labels in the order they are exposed.
    pub fn histogram_count(&self, family: &str, labels: &[(&'static str, &str)]) -> u64 {
        self.family(family).map_or(0, |found| {
            found.count(&labels.iter().map(|(k, v)| (*k, (*v).to_owned())).collect())
        })
    }

    /// The sum of a histogram's observations, in its observed integer unit
    /// (milliseconds, microseconds, thousandths or tokens) — for tests.
    pub fn histogram_sum(&self, family: &str, labels: &[(&'static str, &str)]) -> i64 {
        self.family(family).map_or(0, |found| {
            found.sum(&labels.iter().map(|(k, v)| (*k, (*v).to_owned())).collect())
        })
    }

    fn family(&self, name: &str) -> Option<&Family> {
        let h = &self.histograms;
        [
            &h.request,
            &h.planning,
            &h.ttft,
            &h.upstream_ttft,
            &h.upstream_response,
            &h.upstream_duration,
            &h.estimation_ratio,
            &h.estimation_error,
            &h.reconcile,
        ]
        .into_iter()
        .find(|family| family.name == name)
    }

    /// The affinity book's gauges and eviction counts, in the same format.
    pub fn affinity_to_prometheus(book: &AffinityBook) -> String {
        let mut out = String::new();
        out.push_str(
            "# HELP router_session_affinity_enabled 1 if session affinity is configured on.\n",
        );
        out.push_str("# TYPE router_session_affinity_enabled gauge\n");
        let _ = writeln!(
            out,
            "router_session_affinity_enabled {}",
            u8::from(book.enabled())
        );
        out.push_str(
            "# HELP router_session_affinity_entries Sessions with a live affinity, across routes.\n",
        );
        out.push_str("# TYPE router_session_affinity_entries gauge\n");
        let _ = writeln!(out, "router_session_affinity_entries {}", book.len());
        let (expired, capacity) = book.evictions();
        out.push_str(
            "# HELP router_session_affinity_evictions_total Affinities removed, by reason: idle past the TTL, or to stay within max_entries.\n",
        );
        out.push_str("# TYPE router_session_affinity_evictions_total counter\n");
        let _ = writeln!(
            out,
            "router_session_affinity_evictions_total{{reason=\"expired\"}} {expired}"
        );
        let _ = writeln!(
            out,
            "router_session_affinity_evictions_total{{reason=\"capacity\"}} {capacity}"
        );
        out
    }

    pub fn to_prometheus(
        &self,
        health: &BTreeMap<NodeId, NodeStatus>,
        load: &BTreeMap<DeploymentId, u64>,
    ) -> String {
        let mut out = String::new();

        out.push_str(
            "# HELP router_requests_total Requests the router answered, by route and outcome.\n",
        );
        out.push_str("# TYPE router_requests_total counter\n");
        for ((route, outcome), count) in self
            .requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            let _ = writeln!(
                out,
                "router_requests_total{{route=\"{}\",outcome=\"{}\"}} {count}",
                escape(route),
                outcome.as_str()
            );
        }

        out.push_str(
            "# HELP router_failovers_total Attempts that moved to the next deployment before any response was sent.\n",
        );
        out.push_str("# TYPE router_failovers_total counter\n");
        for (route, count) in self
            .failovers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            let _ = writeln!(
                out,
                "router_failovers_total{{route=\"{}\"}} {count}",
                escape(route)
            );
        }

        out.push_str("# HELP router_active_requests Requests in flight, including open streams.\n");
        out.push_str("# TYPE router_active_requests gauge\n");
        let _ = writeln!(out, "router_active_requests {}", self.active());

        out.push_str(
            "# HELP router_routing_decisions_total Committed routing decisions, by route, policy and reason.\n",
        );
        out.push_str("# TYPE router_routing_decisions_total counter\n");
        for ((route, policy, reason), count) in self
            .decisions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            let _ = writeln!(
                out,
                "router_routing_decisions_total{{route=\"{}\",policy=\"{policy}\",reason=\"{reason}\"}} {count}",
                escape(route)
            );
        }

        out.push_str(
            "# HELP router_capability_filtered_total Available deployments ruled out by a request's requirements, by route and reason.\n",
        );
        out.push_str("# TYPE router_capability_filtered_total counter\n");
        for ((route, reason), count) in self
            .capability_filtered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            let _ = writeln!(
                out,
                "router_capability_filtered_total{{route=\"{}\",reason=\"{reason}\"}} {count}",
                escape(route)
            );
        }

        out.push_str(
            "# HELP router_capability_mismatch_total Requests refused because no available deployment could serve them.\n",
        );
        out.push_str("# TYPE router_capability_mismatch_total counter\n");
        for (route, count) in self
            .capability_mismatches
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            let _ = writeln!(
                out,
                "router_capability_mismatch_total{{route=\"{}\"}} {count}",
                escape(route)
            );
        }

        out.push_str(
            "# HELP router_context_overflow_failovers_total Failovers to a larger-context deployment after context_length_exceeded.\n",
        );
        out.push_str("# TYPE router_context_overflow_failovers_total counter\n");
        for (route, count) in self
            .context_overflow_failovers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            let _ = writeln!(
                out,
                "router_context_overflow_failovers_total{{route=\"{}\"}} {count}",
                escape(route)
            );
        }

        for (name, help, map) in [
            (
                "router_session_affinity_hits_total",
                "Requests whose session's sticky deployment was still valid and went first, by route.",
                &self.affinity_hits,
            ),
            (
                "router_session_affinity_misses_total",
                "Requests naming a session with no usable affinity (new, expired, or its deployment no longer valid), by route.",
                &self.affinity_misses,
            ),
        ] {
            let _ = writeln!(out, "# HELP {name} {help}");
            let _ = writeln!(out, "# TYPE {name} counter");
            for (route, count) in map.lock().unwrap_or_else(PoisonError::into_inner).iter() {
                let _ = writeln!(out, "{name}{{route=\"{}\"}} {count}", escape(route));
            }
        }
        out.push_str(
            "# HELP router_session_affinity_reassignments_total Sessions moved off their sticky deployment, by route and reason.\n",
        );
        out.push_str("# TYPE router_session_affinity_reassignments_total counter\n");
        for ((route, reason), count) in self
            .affinity_reassignments
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            let _ = writeln!(
                out,
                "router_session_affinity_reassignments_total{{route=\"{}\",reason=\"{reason}\"}} {count}",
                escape(route)
            );
        }

        out.push_str(
            "# HELP router_placement_actions_total Placement actions finished, by route, action and result.\n",
        );
        out.push_str("# TYPE router_placement_actions_total counter\n");
        for ((route, action, result), count) in self
            .placement_actions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            let _ = writeln!(
                out,
                "router_placement_actions_total{{route=\"{}\",action=\"{action}\",result=\"{result}\"}} {count}",
                escape(route)
            );
        }
        out.push_str(
            "# HELP router_placement_failures_total Placement actions that failed, by route and reason.\n",
        );
        out.push_str("# TYPE router_placement_failures_total counter\n");
        for ((route, reason), count) in self
            .placement_failures
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            let _ = writeln!(
                out,
                "router_placement_failures_total{{route=\"{}\",reason=\"{reason}\"}} {count}",
                escape(route)
            );
        }

        out.push_str(
            "# HELP router_auto_route_decisions_total Routes Auto chose, by rule and route. `_fallback` is no rule matching.\n",
        );
        out.push_str("# TYPE router_auto_route_decisions_total counter\n");
        for ((rule, route), count) in self
            .auto_decisions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            let _ = writeln!(
                out,
                "router_auto_route_decisions_total{{rule=\"{}\",route=\"{}\"}} {count}",
                escape(rule),
                escape(route)
            );
        }
        out.push_str(
            "# HELP router_auto_route_fallback_total Auto requests no rule matched, by the fallback route they went to.\n",
        );
        out.push_str("# TYPE router_auto_route_fallback_total counter\n");
        for (route, count) in self
            .auto_fallbacks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            let _ = writeln!(
                out,
                "router_auto_route_fallback_total{{route=\"{}\"}} {count}",
                escape(route)
            );
        }

        out.push_str(
            "# HELP router_classifier_requests_total Auto classifications, by provider and outcome: chosen, low_confidence, invalid, unavailable, timeout, auth_error, rate_limited, connection_error, provider_error, nested.\n",
        );
        out.push_str("# TYPE router_classifier_requests_total counter\n");
        for ((provider, outcome), count) in self
            .classifier_outcomes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            let _ = writeln!(
                out,
                "router_classifier_requests_total{{provider=\"{provider}\",outcome=\"{outcome}\"}} {count}"
            );
        }
        out.push_str(
            "# HELP router_classifier_route_total Routes Auto classifications chose and took, by provider and route.\n",
        );
        out.push_str("# TYPE router_classifier_route_total counter\n");
        for ((provider, route), count) in self
            .classifier_routes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            let _ = writeln!(
                out,
                "router_classifier_route_total{{provider=\"{provider}\",route=\"{}\"}} {count}",
                escape(route)
            );
        }

        out.push_str(
            "# HELP router_route_scoring_decisions_total Classifications adaptive scoring contested, by the winning logical route and whether it overrode R9.1's route.\n",
        );
        out.push_str("# TYPE router_route_scoring_decisions_total counter\n");
        for ((route, overrode), count) in self
            .scoring_decisions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            let _ = writeln!(
                out,
                "router_route_scoring_decisions_total{{route=\"{}\",overrode=\"{overrode}\"}} {count}",
                escape(route)
            );
        }
        out.push_str(
            "# HELP router_route_scoring_fallback_total Classifications adaptive scoring left as R9.1 resolved them, by reason: below_threshold, no_verdict, internal_error.\n",
        );
        out.push_str("# TYPE router_route_scoring_fallback_total counter\n");
        for (reason, count) in self
            .scoring_fallbacks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            let _ = writeln!(
                out,
                "router_route_scoring_fallback_total{{reason=\"{reason}\"}} {count}"
            );
        }
        out.push_str(
            "# HELP router_route_history_observations_total Finished requests counted toward route history, by route and outcome. Only success and failure are scored.\n",
        );
        out.push_str("# TYPE router_route_history_observations_total counter\n");
        for ((route, outcome), count) in self
            .route_history_observations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            let _ = writeln!(
                out,
                "router_route_history_observations_total{{route=\"{}\",outcome=\"{outcome}\"}} {count}",
                escape(route)
            );
        }

        let h = &self.histograms;
        for family in [
            &h.request,
            &h.planning,
            &h.ttft,
            &h.upstream_ttft,
            &h.upstream_response,
            &h.upstream_duration,
            &h.estimation_ratio,
            &h.estimation_error,
            &h.reconcile,
            &h.classifier,
        ] {
            family.render(&mut out);
        }

        out.push_str(
            "# HELP router_deployment_active_requests Upstream attempts this router has in flight, per deployment.\n",
        );
        out.push_str("# TYPE router_deployment_active_requests gauge\n");
        for (deployment, active) in load {
            let _ = writeln!(
                out,
                "router_deployment_active_requests{{deployment=\"{}\"}} {active}",
                escape(deployment.as_str())
            );
        }

        out.push_str(
            "# HELP router_node_health 1 if the node is healthy, 0 if not, -1 if not yet known.\n",
        );
        out.push_str("# TYPE router_node_health gauge\n");
        for (node, status) in health {
            let value = match status.health {
                NodeHealth::Healthy => 1,
                NodeHealth::Unhealthy => 0,
                NodeHealth::Unknown => -1,
            };
            let _ = writeln!(
                out,
                "router_node_health{{node=\"{}\"}} {value}",
                escape(node.as_str())
            );
        }
        out
    }
}

fn bump<K: Ord>(map: &Mutex<BTreeMap<K, u64>>, key: K) {
    *map.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .entry(key)
        .or_default() += 1;
}

fn read<K: Ord>(map: &Mutex<BTreeMap<K, u64>>, key: &K) -> u64 {
    map.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(key)
        .copied()
        .unwrap_or_default()
}

fn route_policy(route: &str, policy: &'static str) -> Labels {
    vec![("route", route.to_owned()), ("policy", policy.to_owned())]
}

fn route_deployment(route: &str, deployment: &str) -> Labels {
    vec![
        ("route", route.to_owned()),
        ("deployment", deployment.to_owned()),
    ]
}

/// Escape a label value per the exposition format.
fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_and_the_gauge_render() {
        let metrics = RouterMetrics::default();
        metrics.record_request("Coder", Outcome::Ok);
        metrics.record_request("Coder", Outcome::Ok);
        metrics.record_failover("Coder");
        metrics.record_capability_filtered("Coder", "tools_unsupported");
        metrics.record_capability_mismatch("Coder");
        metrics.record_context_overflow_failover("Coder");
        let guard = metrics.enter();
        let text = metrics.to_prometheus(&BTreeMap::new(), &BTreeMap::new());
        assert!(text.contains("router_requests_total{route=\"Coder\",outcome=\"ok\"} 2"));
        assert!(text.contains("router_failovers_total{route=\"Coder\"} 1"));
        assert!(text.contains("router_active_requests 1"));
        assert!(text.contains(
            "router_capability_filtered_total{route=\"Coder\",reason=\"tools_unsupported\"} 1"
        ));
        assert!(text.contains("router_capability_mismatch_total{route=\"Coder\"} 1"));
        assert!(text.contains("router_context_overflow_failovers_total{route=\"Coder\"} 1"));
        drop(guard);
        assert_eq!(metrics.active(), 0);
    }

    #[test]
    fn histograms_render_cumulative_buckets_in_seconds() {
        let metrics = RouterMetrics::default();
        metrics.observe_ttft("Coder", "round_robin", Duration::from_millis(40));
        metrics.observe_ttft("Coder", "round_robin", Duration::from_millis(400));
        metrics.observe_planning("Coder", "round_robin", Duration::from_micros(30));
        let text = metrics.to_prometheus(&BTreeMap::new(), &BTreeMap::new());
        assert!(text.contains("# TYPE router_ttft_seconds histogram"));
        assert!(text.contains(
            "router_ttft_seconds_bucket{route=\"Coder\",policy=\"round_robin\",le=\"0.05\"} 1"
        ));
        assert!(text.contains(
            "router_ttft_seconds_bucket{route=\"Coder\",policy=\"round_robin\",le=\"0.5\"} 2"
        ));
        assert!(text.contains(
            "router_ttft_seconds_bucket{route=\"Coder\",policy=\"round_robin\",le=\"+Inf\"} 2"
        ));
        assert!(
            text.contains("router_ttft_seconds_sum{route=\"Coder\",policy=\"round_robin\"} 0.44")
        );
        assert!(
            text.contains("router_ttft_seconds_count{route=\"Coder\",policy=\"round_robin\"} 2")
        );
        assert!(text.contains(
            "router_routing_duration_seconds_bucket{route=\"Coder\",policy=\"round_robin\",le=\"0.00005\"} 1"
        ));
    }

    #[test]
    fn the_estimate_comparison_never_divides_by_zero() {
        let metrics = RouterMetrics::default();
        // Underestimated by a factor of three.
        metrics.observe_estimate("Coder", 100, 300);
        // Exact.
        metrics.observe_estimate("Coder", 50, 50);
        // An empty prompt: no ratio, but the (zero) error is still a fact.
        metrics.observe_estimate("Coder", 0, 0);
        metrics.observe_estimate("Coder", 0, 7);
        let route = [("route", "Coder")];
        assert_eq!(
            metrics.histogram_count("router_context_estimation_ratio", &route),
            2
        );
        assert_eq!(
            metrics.histogram_sum("router_context_estimation_ratio", &route),
            4_000
        );
        assert_eq!(
            metrics.histogram_count("router_context_estimation_error_tokens", &route),
            4
        );
        assert_eq!(
            metrics.histogram_sum("router_context_estimation_error_tokens", &route),
            207
        );
        let text = metrics.to_prometheus(&BTreeMap::new(), &BTreeMap::new());
        assert!(!text.contains("inf\n") && !text.contains("NaN"));
        assert!(text.contains(
            "router_context_estimation_error_tokens_bucket{route=\"Coder\",le=\"-16\"} 0"
        ));
    }

    #[test]
    fn affinity_counters_carry_only_route_and_reason() {
        let metrics = RouterMetrics::default();
        metrics.record_affinity_hit("Coder");
        metrics.record_affinity_miss("Coder");
        metrics.record_affinity_reassignment("Coder", "sticky_unhealthy");
        let text = metrics.to_prometheus(&BTreeMap::new(), &BTreeMap::new());
        assert!(text.contains("router_session_affinity_hits_total{route=\"Coder\"} 1"));
        assert!(text.contains("router_session_affinity_misses_total{route=\"Coder\"} 1"));
        assert!(text.contains(
            "router_session_affinity_reassignments_total{route=\"Coder\",reason=\"sticky_unhealthy\"} 1"
        ));
        let book = AffinityBook::new(crate::config::AffinityPolicy::default());
        let gauges = RouterMetrics::affinity_to_prometheus(&book);
        assert!(gauges.contains("router_session_affinity_entries 0"));
        assert!(gauges.contains("router_session_affinity_enabled 0"));
    }

    #[test]
    fn an_unlabelled_histogram_renders_without_empty_braces() {
        let metrics = RouterMetrics::default();
        metrics.observe_reconcile(Duration::from_micros(40));
        metrics.record_placement_action("Coder", "load", "succeeded");
        metrics.record_placement_failure("Coder", "admission_failed");
        let text = metrics.to_prometheus(&BTreeMap::new(), &BTreeMap::new());
        assert!(
            text.contains("router_placement_reconcile_duration_seconds_bucket{le=\"0.00005\"} 1")
        );
        assert!(text.contains("router_placement_reconcile_duration_seconds_count 1"));
        assert!(!text.contains("{}"));
        assert!(text.contains(
            "router_placement_actions_total{route=\"Coder\",action=\"load\",result=\"succeeded\"} 1"
        ));
        assert!(text.contains(
            "router_placement_failures_total{route=\"Coder\",reason=\"admission_failed\"} 1"
        ));
    }

    #[test]
    fn a_quote_in_a_route_name_cannot_break_the_format() {
        assert_eq!(escape("say \"hi\""), "say \\\"hi\\\"");
    }
}
