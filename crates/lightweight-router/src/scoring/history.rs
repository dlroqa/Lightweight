//! Route history: one small, decaying aggregate per logical route.
//!
//! **Observational only in R9.2 slice 1.** Everything here is recorded,
//! decayed, shown (admin view, traces, metrics) and resettable, and nothing
//! here enters a score: [`super::decide`] reads only [`HistoryObservations`]
//! for its trace. The estimator below ([`signal_of`], [`HistorySignal`]) is
//! kept as the foundation for a future route-quality signal and is not read by
//! routing: with only successes attributable, it measures successful traffic
//! volume, and popularity must never masquerade as quality.
//!
//! Recorded once per finished request, for the route that handled it — direct
//! and `Auto` traffic alike — and read by [`super::decide`]. Never per request,
//! never per deployment, node or session, never a prompt: one decayed count
//! and a few counters per configured route, in memory only. A router restart,
//! or `POST /api/router/v1/adaptive-scoring/reset`, starts it from nothing.
//!
//! **Observation is not scoring.** Every final outcome is recorded and shown;
//! only an outcome that can be attributed to the *logical route* — rather than
//! to the one deployment, node or connection that happened to serve the
//! request — could ever enter a quality estimate, and in slice 1 none enters
//! any score at all (see above). History is meant to answer "was this route a good
//! choice?", never "did the node picked this time behave?": that question is
//! health's and the route policy's.
//!
//! | final outcome | observation | an estimator sample? |
//! |---|---|---|
//! | `ok` (a completed response or stream) | `success` | **yes** |
//! | `server_error` (a 5xx answer: one deployment's, as 500 is never retried) | `server_error` | no |
//! | `interrupted` (a committed stream one node or its connection broke off) | `interrupted` | no |
//! | `unavailable`, or every deployment refused 502/503/504 before answering | `unavailable` | no |
//! | `route_capability_mismatch` | `mismatch` | no |
//! | `client_error`, `cancelled` | `neutral` | no |
//!
//! Every failure the router can see today is one deployment's or one
//! connection's, so only successes are samples, and no negative evidence is
//! manufactured. Successes alone measure volume, which is exactly why the
//! estimator is not used for routing yet.
//!
//! The future estimator ([`signal_of`], not read by routing). `n` is the
//! decayed count of samples — successes — and nothing else: other outcomes
//! never bring a route closer to `min_samples`.
//!
//! ```text
//! n < min_samples   →  h = 0                        (neutral; "gated")
//! otherwise         →  ŝ = (s + k/2) / (n + k)      (shrunk toward 0.5; s = n here)
//!                       h = 2·ŝ − 1  =  n / (n + k)  (in [0, 1))
//! ```
//!
//! The count decays continuously with a half-life (`2^(−Δt / half_life)`), so a
//! route that stops receiving traffic drifts back below `min_samples` and to
//! neutral on its own.

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::Serialize;

use super::HistoryPolicy;
use crate::domain::RouteName;

/// The success rate that means "no evidence either way".
pub const NEUTRAL_SUCCESS_RATE: f64 = 0.5;

/// How one finished request counts toward its route's history.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Observation {
    /// A completed response or stream. Scored.
    Success,
    /// A 5xx answer. Observed, never scored: 500 is not retried, so it is one
    /// deployment's answer, and says nothing about the route's others.
    ServerError,
    /// A committed stream one node or its connection broke off. Observed,
    /// never scored, for the same reason.
    Interrupted,
    /// Nothing ready could take it. Observed, never scored.
    Unavailable,
    /// No available deployment could serve the request
    /// (`route_capability_mismatch`). Observed, never scored.
    CapabilityMismatch,
    /// The request or the client: a client error, or a client that left.
    Neutral,
}

impl Observation {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::ServerError => "server_error",
            Self::Interrupted => "interrupted",
            Self::Unavailable => "unavailable",
            Self::CapabilityMismatch => "mismatch",
            Self::Neutral => "neutral",
        }
    }

    /// Whether it enters the scored signal (and `effective_samples`).
    pub const fn scored(self) -> bool {
        matches!(self, Self::Success)
    }

    /// The observation a finished request's trace outcome makes: `ok`,
    /// `server_error`, `interrupted`, `unavailable`, `client_error` or
    /// `cancelled`. A capability mismatch is a `client_error` to the client
    /// and is told apart by the caller.
    pub fn of_outcome(outcome: &str) -> Self {
        match outcome {
            "ok" => Self::Success,
            "server_error" => Self::ServerError,
            "interrupted" => Self::Interrupted,
            "unavailable" => Self::Unavailable,
            _ => Self::Neutral,
        }
    }
}

/// What has been observed of a route: decayed scored-category samples. Shown in
/// traces and the admin view; never a score input.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct HistoryObservations {
    /// Decayed successful completions — the only category a future quality
    /// estimator could count today.
    pub effective_samples: f64,
    pub successes: f64,
    /// At least `min_samples` effective samples.
    pub min_samples_reached: bool,
}

impl HistoryObservations {
    pub const NONE: Self = Self {
        effective_samples: 0.0,
        successes: 0.0,
        min_samples_reached: false,
    };
}

/// A future quality estimator over a route's history. **Not read by routing in
/// slice 1**: kept, and tested, as the foundation for a signal that becomes
/// eligible only with genuinely route-attributable quality evidence.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct HistorySignal {
    /// Decayed **scored** observations only: what `min_samples` gates on.
    pub effective_samples: f64,
    /// Decayed successes (the only scored observation in slice 1).
    pub successes: f64,
    /// The shrunk estimate `(s + k/2) / (n + k)`.
    pub success_rate: f64,
    /// The history term: `2·success_rate − 1`, or 0 while `gated`.
    pub value: f64,
    /// Fewer than `min_samples` effective samples: the value is neutral.
    pub gated: bool,
}

impl HistorySignal {
    /// No evidence: neutral.
    pub const NEUTRAL: Self = Self {
        effective_samples: 0.0,
        successes: 0.0,
        success_rate: NEUTRAL_SUCCESS_RATE,
        value: 0.0,
        gated: true,
    };
}

/// The signal decayed scored successes give, under `policy`.
pub fn signal_of(successes: f64, policy: &HistoryPolicy) -> HistorySignal {
    let n = successes;
    let k = f64::from(policy.shrinkage_samples);
    let success_rate = (successes + k * NEUTRAL_SUCCESS_RATE) / (n + k);
    let gated = n < f64::from(policy.min_samples);
    HistorySignal {
        effective_samples: n,
        successes,
        success_rate,
        value: if gated { 0.0 } else { 2.0 * success_rate - 1.0 },
        gated,
    }
}

fn observations_of(successes: f64, policy: &HistoryPolicy) -> HistoryObservations {
    HistoryObservations {
        effective_samples: successes,
        successes,
        min_samples_reached: successes >= f64::from(policy.min_samples),
    }
}

/// What a count is worth `elapsed` later: `2^(−elapsed / half_life)`.
pub fn decay_factor(elapsed: Duration, half_life: Duration) -> f64 {
    (-elapsed.as_secs_f64() / half_life.as_secs_f64()).exp2()
}

#[derive(Clone, Debug, Default)]
struct Record {
    /// Decayed scored successes.
    successes: f64,
    /// When `successes` was last brought up to date.
    decayed_at: Option<Instant>,
    /// Observed, never scored.
    server_error: u64,
    interrupted: u64,
    unavailable: u64,
    mismatch: u64,
    neutral: u64,
    /// Unix seconds of the last observation of any kind.
    last_observed_at: Option<u64>,
}

impl Record {
    /// The scored count as it stands at `now`.
    fn decayed(&self, now: Instant, half_life: Duration) -> f64 {
        let factor = self.decayed_at.map_or(1.0, |at| {
            decay_factor(now.saturating_duration_since(at), half_life)
        });
        self.successes * factor
    }
}

/// One route's history, for the admin view. Numbers and a route name only;
/// observations, never a score.
#[derive(Clone, Debug, Serialize)]
pub struct RouteHistoryView {
    pub route: String,
    #[serde(flatten)]
    pub observations: HistoryObservations,
    pub server_error: u64,
    pub interrupted: u64,
    pub unavailable: u64,
    pub mismatch: u64,
    pub neutral: u64,
    pub last_observed_at: Option<u64>,
}

/// Every configured route's history.
///
/// Records nothing while adaptive scoring is off: then it has no policy, and
/// the router behaves exactly as it did before scoring existed.
#[derive(Debug)]
pub struct HistoryBook {
    policy: Option<HistoryPolicy>,
    routes: Vec<RouteName>,
    records: Mutex<Vec<Record>>,
}

impl HistoryBook {
    /// A book for `routes`, recording under `policy` when there is one.
    pub fn new(routes: Vec<RouteName>, policy: Option<HistoryPolicy>) -> Self {
        let records = Mutex::new(vec![Record::default(); routes.len()]);
        Self {
            policy,
            routes,
            records,
        }
    }

    /// Whether finished requests are recorded.
    pub const fn is_recording(&self) -> bool {
        self.policy.is_some()
    }

    fn index(&self, route: &RouteName) -> Option<usize> {
        self.routes.iter().position(|known| known == route)
    }

    /// The configured route `requested` names, matched as route names are.
    pub fn route_named(&self, requested: &str) -> Option<&RouteName> {
        self.routes.iter().find(|route| route.matches(requested))
    }

    /// Count one finished request against `route`. Returns whether it was
    /// recorded: never while scoring is off, never for an unknown route.
    pub fn observe(&self, route: &RouteName, observation: Observation, now: Instant) -> bool {
        let (Some(policy), Some(index)) = (self.policy, self.index(route)) else {
            return false;
        };
        let mut records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(record) = records.get_mut(index) else {
            return false;
        };
        match observation {
            Observation::Success => {
                record.successes = record.decayed(now, policy.half_life) + 1.0;
                record.decayed_at = Some(now);
            }
            Observation::ServerError => record.server_error += 1,
            Observation::Interrupted => record.interrupted += 1,
            Observation::Unavailable => record.unavailable += 1,
            Observation::CapabilityMismatch => record.mismatch += 1,
            Observation::Neutral => record.neutral += 1,
        }
        record.last_observed_at = Some(crate::classifier::unix_now());
        true
    }

    /// `route`'s future-estimator signal at `now` (not read by routing in
    /// slice 1). Neutral for a route with no history, an
    /// unknown route, or while scoring is off.
    pub fn signal(&self, route: &RouteName, now: Instant) -> HistorySignal {
        let (Some(policy), Some(index)) = (self.policy, self.index(route)) else {
            return HistorySignal::NEUTRAL;
        };
        let records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        records.get(index).map_or(HistorySignal::NEUTRAL, |record| {
            signal_of(record.decayed(now, policy.half_life), &policy)
        })
    }

    /// What has been observed of `route` at `now`. Empty for a route with no
    /// history, an unknown route, or while scoring is off.
    pub fn observations(&self, route: &RouteName, now: Instant) -> HistoryObservations {
        let (Some(policy), Some(index)) = (self.policy, self.index(route)) else {
            return HistoryObservations::NONE;
        };
        let records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        records
            .get(index)
            .map_or(HistoryObservations::NONE, |record| {
                observations_of(record.decayed(now, policy.half_life), &policy)
            })
    }

    /// Forget `route`'s history, or every route's. Returns the routes reset.
    /// Nothing else is touched: not a route, a rule, a classifier setting, a
    /// session's affinity, placement or a node.
    pub fn reset(&self, route: Option<&RouteName>) -> Vec<RouteName> {
        let mut records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        match route {
            Some(route) => match self.index(route) {
                Some(index) => {
                    if let Some(record) = records.get_mut(index) {
                        *record = Record::default();
                    }
                    vec![route.clone()]
                }
                None => Vec::new(),
            },
            None => {
                records.fill(Record::default());
                self.routes.clone()
            }
        }
    }

    /// Every route's history at `now`, in configured order.
    pub fn view(&self, now: Instant) -> Vec<RouteHistoryView> {
        let records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        self.routes
            .iter()
            .zip(records.iter())
            .map(|(route, record)| RouteHistoryView {
                route: route.to_string(),
                observations: self.policy.map_or(HistoryObservations::NONE, |policy| {
                    observations_of(record.decayed(now, policy.half_life), &policy)
                }),
                server_error: record.server_error,
                interrupted: record.interrupted,
                unavailable: record.unavailable,
                mismatch: record.mismatch,
                neutral: record.neutral,
                last_observed_at: record.last_observed_at,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> HistoryPolicy {
        HistoryPolicy {
            half_life: Duration::from_secs(3_600),
            min_samples: 20,
            shrinkage_samples: 20,
        }
    }

    fn name(raw: &str) -> RouteName {
        RouteName::parse(raw).unwrap()
    }

    fn book() -> HistoryBook {
        HistoryBook::new(
            vec![name("General"), name("Coder"), name("Research")],
            Some(policy()),
        )
    }

    fn observe_n(book: &HistoryBook, route: &str, observation: Observation, n: u32, at: Instant) {
        for _ in 0..n {
            assert!(book.observe(&name(route), observation, at));
        }
    }

    const UNSCORED: [Observation; 5] = [
        Observation::ServerError,
        Observation::Interrupted,
        Observation::Unavailable,
        Observation::CapabilityMismatch,
        Observation::Neutral,
    ];

    #[test]
    fn no_history_is_neutral() {
        let book = book();
        let now = Instant::now();
        assert_eq!(book.signal(&name("Coder"), now), HistorySignal::NEUTRAL);
        assert_eq!(
            book.signal(&name("Nowhere"), now),
            HistorySignal::NEUTRAL,
            "an unknown route is neutral too"
        );
    }

    #[test]
    fn below_min_samples_is_neutral_and_at_min_samples_is_shrunk() {
        let book = book();
        let now = Instant::now();
        observe_n(&book, "Coder", Observation::Success, 19, now);
        let below = book.signal(&name("Coder"), now);
        assert!(below.gated);
        assert_eq!(below.value, 0.0, "19 successes are not yet evidence");
        assert!((below.effective_samples - 19.0).abs() < 1e-9);

        observe_n(&book, "Coder", Observation::Success, 1, now);
        let at = book.signal(&name("Coder"), now);
        assert!(!at.gated);
        // 20 successes shrunk with k = 20: (20 + 10) / (20 + 20) = 0.75, not 1.0.
        assert!((at.success_rate - 0.75).abs() < 1e-12, "{at:?}");
        assert!((at.value - 0.5).abs() < 1e-12, "{at:?}");
    }

    #[test]
    fn success_raises_the_signal_within_its_bound() {
        let book = book();
        let now = Instant::now();
        observe_n(&book, "Coder", Observation::Success, 10_000, now);
        let good = book.signal(&name("Coder"), now);
        assert!(good.value > 0.99 && good.value < 1.0, "{good:?}");
        assert_eq!(
            book.signal(&name("General"), now),
            HistorySignal::NEUTRAL,
            "one route's history is not another's"
        );
    }

    #[test]
    fn only_success_is_scored() {
        assert!(Observation::Success.scored());
        for observation in UNSCORED {
            assert!(!observation.scored(), "{observation:?}");
        }
    }

    #[test]
    fn one_server_error_leaves_the_signal_unchanged_and_is_counted() {
        let book = book();
        let now = Instant::now();
        observe_n(&book, "Coder", Observation::Success, 25, now);
        let before = book.signal(&name("Coder"), now);
        observe_n(&book, "Coder", Observation::ServerError, 1, now);
        assert_eq!(book.signal(&name("Coder"), now), before);
        assert_eq!(book.view(now)[1].server_error, 1);
    }

    #[test]
    fn many_unscored_outcomes_never_move_the_signal() {
        let book = book();
        let now = Instant::now();
        for observation in UNSCORED {
            observe_n(&book, "Research", observation, 1_000, now);
        }
        assert_eq!(
            book.signal(&name("Research"), now),
            HistorySignal::NEUTRAL,
            "server errors, interruptions, unavailability, mismatches, client errors and \
             cancellations are not evidence about the route"
        );
        let view = &book.view(now)[2];
        assert_eq!(
            (
                view.server_error,
                view.interrupted,
                view.unavailable,
                view.mismatch,
                view.neutral
            ),
            (1_000, 1_000, 1_000, 1_000, 1_000),
            "and every one is still visible"
        );
        assert!(view.last_observed_at.is_some());
    }

    #[test]
    fn unscored_outcomes_never_count_toward_min_samples() {
        // 1 scored success and 19 server errors is one sample, not twenty.
        let book = book();
        let now = Instant::now();
        observe_n(&book, "Coder", Observation::Success, 1, now);
        observe_n(&book, "Coder", Observation::ServerError, 19, now);
        let signal = book.signal(&name("Coder"), now);
        assert!((signal.effective_samples - 1.0).abs() < 1e-12, "{signal:?}");
        assert!(signal.gated);
        assert_eq!(signal.value, 0.0);
        // Nor does any other unscored outcome.
        for observation in UNSCORED {
            observe_n(&book, "Coder", observation, 100, now);
        }
        assert!(book.signal(&name("Coder"), now).gated);
        // Only scored successes open the gate.
        observe_n(&book, "Coder", Observation::Success, 19, now);
        assert!(!book.signal(&name("Coder"), now).gated);
    }

    #[test]
    fn trace_outcomes_map_to_observations() {
        assert_eq!(Observation::of_outcome("ok"), Observation::Success);
        assert_eq!(
            Observation::of_outcome("server_error"),
            Observation::ServerError
        );
        assert_eq!(
            Observation::of_outcome("interrupted"),
            Observation::Interrupted
        );
        assert_eq!(
            Observation::of_outcome("unavailable"),
            Observation::Unavailable
        );
        assert_eq!(
            Observation::of_outcome("client_error"),
            Observation::Neutral
        );
        assert_eq!(Observation::of_outcome("cancelled"), Observation::Neutral);
    }

    #[test]
    fn counts_halve_every_half_life() {
        let half_life = Duration::from_secs(3_600);
        assert!((decay_factor(half_life, half_life) - 0.5).abs() < 1e-12);
        assert!((decay_factor(half_life * 2, half_life) - 0.25).abs() < 1e-12);
        assert_eq!(decay_factor(Duration::ZERO, half_life), 1.0);

        let book = book();
        let start = Instant::now();
        observe_n(&book, "Coder", Observation::Success, 80, start);
        let later = book.signal(&name("Coder"), start + half_life);
        assert!(
            (later.effective_samples - 40.0).abs() < 1e-9,
            "the one-hour default halves: {later:?}"
        );
    }

    #[test]
    fn a_quiet_route_returns_to_neutral() {
        let book = book();
        let start = Instant::now();
        observe_n(&book, "Coder", Observation::Success, 80, start);
        let fresh = book.signal(&name("Coder"), start);
        assert!(fresh.value > 0.5, "{fresh:?}");
        // No more traffic: 80 → 40 → 20 → 10 samples.
        let one = book.signal(&name("Coder"), start + Duration::from_secs(3_600));
        assert!(one.value < fresh.value && one.value > 0.0, "{one:?}");
        let three = book.signal(&name("Coder"), start + Duration::from_secs(3 * 3_600));
        assert!(three.gated, "below min_samples again: {three:?}");
        assert_eq!(three.value, 0.0, "back to neutral, with no exploration");
    }

    #[test]
    fn a_custom_half_life_is_used() {
        let book = HistoryBook::new(
            vec![name("Coder")],
            Some(HistoryPolicy {
                half_life: Duration::from_secs(60),
                ..policy()
            }),
        );
        let start = Instant::now();
        observe_n(&book, "Coder", Observation::Success, 64, start);
        let later = book.signal(&name("Coder"), start + Duration::from_secs(120));
        assert!((later.effective_samples - 16.0).abs() < 1e-9, "{later:?}");
    }

    #[test]
    fn observations_decay_what_came_before() {
        let book = book();
        let start = Instant::now();
        observe_n(&book, "Coder", Observation::Success, 40, start);
        observe_n(
            &book,
            "Coder",
            Observation::Success,
            40,
            start + Duration::from_secs(3_600),
        );
        let signal = book.signal(&name("Coder"), start + Duration::from_secs(3_600));
        assert!((signal.successes - 60.0).abs() < 1e-9, "{signal:?}");
    }

    #[test]
    fn a_large_sample_count_never_reaches_the_bound() {
        let book = book();
        let now = Instant::now();
        observe_n(&book, "Coder", Observation::Success, 100_000, now);
        let signal = book.signal(&name("Coder"), now);
        assert!(signal.value < 1.0, "{signal:?}");
        assert!(signal.success_rate < 1.0);
    }

    #[test]
    fn reset_clears_one_route_or_all() {
        let book = book();
        let now = Instant::now();
        for route in ["General", "Coder", "Research"] {
            observe_n(&book, route, Observation::Success, 30, now);
            observe_n(&book, route, Observation::ServerError, 3, now);
        }
        assert_eq!(book.reset(Some(&name("Coder"))), vec![name("Coder")]);
        assert_eq!(book.signal(&name("Coder"), now), HistorySignal::NEUTRAL);
        assert_eq!(book.view(now)[1].server_error, 0);
        assert!(!book.signal(&name("General"), now).gated, "untouched");

        assert!(book.reset(Some(&name("Nowhere"))).is_empty());
        assert_eq!(book.reset(None).len(), 3);
        for view in book.view(now) {
            assert_eq!(view.observations, HistoryObservations::NONE);
            assert_eq!(view.last_observed_at, None);
        }
    }

    #[test]
    fn nothing_is_recorded_while_scoring_is_off() {
        let book = HistoryBook::new(vec![name("Coder")], None);
        assert!(!book.is_recording());
        assert!(!book.observe(&name("Coder"), Observation::Success, Instant::now()));
        assert_eq!(book.view(Instant::now())[0].neutral, 0);
    }

    #[test]
    fn routes_are_found_as_route_names_are() {
        let book = book();
        assert_eq!(
            book.route_named(" coder ").map(RouteName::as_str),
            Some("Coder")
        );
        assert_eq!(book.route_named("Auto"), None);
    }
}
