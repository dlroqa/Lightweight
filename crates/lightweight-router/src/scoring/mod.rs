//! Adaptive logical-route scoring (R9.2): among the logical routes a
//! classification leaves plausible, which one should win.
//!
//! **Scoring ranks logical routes only. It never ranks or chooses
//! deployments.** It runs between the classifier and the router, at one
//! place — [`crate::proxy`]'s `Auto` resolution — and its whole output is one
//! route name. Health, capability filtering, session affinity and the route's
//! policy then choose a deployment exactly as for a client that named the route.
//!
//! ```text
//! explicit route ──────────────────────────────────────┐ (never scored)
//! Auto → deterministic rule ───────────────────────────┤ (never scored)
//! Auto → classifying rule → R9.1 classification        │
//!          │  no verdict ─────────── fallback ─────────┤ (not scored: no_verdict)
//!          │  below threshold ────── fallback ─────────┤ (not scored: below_threshold)
//!          ▼  accepted verdict                         │
//!        R9.2: verdict route vs fallback route ────────┤ (scored)
//!                                                      ▼
//!                              one logical route → health → R5 → affinity → policy
//! ```
//!
//! # What is scored
//!
//! Only when the classifier **accepted** a verdict (`confidence ≥
//! min_confidence`). Then two routes contend, and only two, because a
//! classifier says one route and one confidence and nothing about the others:
//!
//! * the **verdict route**, whose classifier signal is the verdict's
//!   confidence; and
//! * the **fallback route**, whose classifier signal is the
//!   [`classifier_baseline`] — the provider's own `min_confidence`, the bar the
//!   verdict had to clear to be accepted at all.
//!
//! ```text
//! score(route) = W_classifier · classifier_signal(route)
//!              + W_prior      · prior(route)                 ∈ [0, 1]
//!              + W_history    · history(route)               ∈ (−1, 1)
//! ```
//!
//! The verdict wins ties, which is R9.1's own `confidence ≥ min_confidence`
//! rule at equality. With `W_prior = W_history = 0` the verdict's
//! `W·confidence ≥ W·baseline` always holds, so the result is R9.1's, request
//! for request.
//!
//! # What is never scored
//!
//! * A verdict **below** the threshold. R9.1 rejected it and its fallback is
//!   the accepted decision; scoring starts from that decision and the rejected
//!   route cannot come back, whatever its prior or history.
//! * A classification with **no** verdict (a timeout, a refused key, …).
//! * A candidate the classifier did not name: there is no signal for it, and
//!   none is invented.
//! * An explicit route, a deterministic rule's route, and `Auto`'s plain
//!   fallback: none of them classify, so none of them reach this module.
//!
//! # How far scoring can move a decision
//!
//! The verdict wins whenever `confidence − baseline ≥ (W_prior·Δprior +
//! W_history·Δhistory) / W_classifier`, and the right side is at most the
//! [`Weights::influence_radius`] `(W_prior + 2·W_history) / W_classifier`.
//! Validation keeps that radius under half of the accepted range
//! `[baseline, 1]`, for every configured provider's threshold
//! ([`MAX_INFLUENCE_SHARE`]): a classification in the upper half of the
//! accepted range is never overturned, by any prior or any history.

pub mod history;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use lightweight_catalog::alias;
use serde::{Deserialize, Serialize};

use crate::auto_route::is_auto;
use crate::classifier::{Classification, ClassifierOutcome, RouteClassifier};
use crate::config::ConfigError;
use crate::domain::RouteName;

pub use history::{HistoryBook, HistorySignal, Observation, RouteHistoryView};

/// One hour: an operational starting point for how fast history fades, not
/// an empirically tuned value.
pub const DEFAULT_HALF_LIFE_SECS: u64 = 3_600;
pub const MIN_HALF_LIFE_SECS: u64 = 60;
pub const MAX_HALF_LIFE_SECS: u64 = 604_800;
/// Effective samples below which a route's history is neutral.
pub const DEFAULT_MIN_SAMPLES: u32 = 20;
/// The bound on `min_samples` and `shrinkage_samples`.
pub const MAX_SAMPLES: u32 = 10_000;
/// The share of the accepted confidence range `[baseline, 1]` priors and
/// history together may reach into. Above it, the classifier alone decides.
pub const MAX_INFLUENCE_SHARE: f64 = 0.5;

const fn default_classifier_weight() -> f64 {
    1.0
}
const fn default_half_life_secs() -> u64 {
    DEFAULT_HALF_LIFE_SECS
}
const fn default_min_samples() -> u32 {
    DEFAULT_MIN_SAMPLES
}

/// The `auto_route.adaptive_scoring` section, as written.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdaptiveScoringFile {
    /// Off unless turned on. A section that is present but off is still
    /// checked.
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub weights: WeightsFile,
    /// A preference per route, in `[0, 1]`. A route not listed has 0.
    #[serde(default)]
    pub priors: BTreeMap<String, f64>,
    #[serde(default)]
    pub history: HistoryFile,
}

/// One set of weights, shared by every classifier provider.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WeightsFile {
    #[serde(default = "default_classifier_weight")]
    pub classifier: f64,
    #[serde(default)]
    pub prior: f64,
    #[serde(default)]
    pub history: f64,
}

impl Default for WeightsFile {
    fn default() -> Self {
        Self {
            classifier: default_classifier_weight(),
            prior: 0.0,
            history: 0.0,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryFile {
    #[serde(default = "default_half_life_secs")]
    pub half_life_secs: u64,
    #[serde(default = "default_min_samples")]
    pub min_samples: u32,
    /// Pseudo-samples a success rate is shrunk toward 0.5 by. Absent: the
    /// same as `min_samples`.
    #[serde(default)]
    pub shrinkage_samples: Option<u32>,
}

impl Default for HistoryFile {
    fn default() -> Self {
        Self {
            half_life_secs: DEFAULT_HALF_LIFE_SECS,
            min_samples: DEFAULT_MIN_SAMPLES,
            shrinkage_samples: None,
        }
    }
}

/// The three weights.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Weights {
    pub classifier: f64,
    pub prior: f64,
    pub history: f64,
}

impl Weights {
    /// How far, in classifier confidence, priors and history together can
    /// move a decision: `(prior + 2·history) / classifier`. A prior spans
    /// `[0, 1]` and a history term `(−1, 1)`, so the largest swing between two
    /// routes is `prior + 2·history`.
    pub fn influence_radius(&self) -> f64 {
        (self.prior + 2.0 * self.history) / self.classifier
    }
}

/// How history is kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HistoryPolicy {
    pub half_life: Duration,
    pub min_samples: u32,
    pub shrinkage_samples: u32,
}

/// The validated section.
#[derive(Clone, Debug)]
pub struct AdaptiveScoring {
    pub enabled: bool,
    pub weights: Weights,
    /// Spelled as the routes are configured, in configured order.
    pub priors: Vec<(RouteName, f64)>,
    pub history: HistoryPolicy,
}

impl AdaptiveScoring {
    /// `route`'s configured prior; 0 when it has none.
    pub fn prior(&self, route: &RouteName) -> f64 {
        self.priors
            .iter()
            .find(|(name, _)| name == route)
            .map_or(0.0, |(_, prior)| *prior)
    }

    /// The configuration, as the admin view shows it.
    pub fn view(&self) -> serde_json::Value {
        serde_json::json!({
            "enabled": self.enabled,
            "weights": self.weights,
            "influence_radius": self.weights.influence_radius(),
            "priors": self.priors.iter()
                .map(|(route, prior)| (route.to_string(), *prior))
                .collect::<BTreeMap<_, _>>(),
            "history": {
                "half_life_secs": self.history.half_life.as_secs(),
                "half_life_provisional": true,
                "min_samples": self.history.min_samples,
                "shrinkage_samples": self.history.shrinkage_samples,
            },
        })
    }
}

/// The classifier signal the fallback route is given: the active provider's
/// own `min_confidence`.
///
/// It is R9.1's decision rule restated as a score. A verdict is accepted only
/// at or above this bar, so in a contest the fallback stands exactly where an
/// accepted verdict has to be to beat it. It is used only for the fallback,
/// only once a verdict was accepted; a verdict below it never contends.
pub fn classifier_baseline(classifier: &RouteClassifier) -> f64 {
    classifier.limits().min_confidence
}

/// Why scoring resolved the way it did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScoringReason {
    /// The accepted verdict and the fallback were scored.
    Scored,
    /// The accepted verdict is the fallback route itself: nothing to contest.
    Uncontested,
    /// R9.1 rejected the verdict as below `min_confidence`; its fallback is the
    /// accepted decision and the rejected route is not eligible.
    BelowThreshold,
    /// The classification produced no verdict; R9.1's fallback stands.
    NoVerdict,
    /// A score was not a finite number; R9.1's route stands. Unreachable with
    /// a validated configuration.
    InternalError,
}

impl ScoringReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Scored => "scored",
            Self::Uncontested => "uncontested",
            Self::BelowThreshold => "below_threshold",
            Self::NoVerdict => "no_verdict",
            Self::InternalError => "internal_error",
        }
    }

    /// Whether routes actually contended: otherwise R9.1's route was used as
    /// it stood.
    pub const fn contested(self) -> bool {
        matches!(self, Self::Scored | Self::Uncontested)
    }
}

/// Where a contender's classifier signal came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClassifierBasis {
    /// The verdict's own confidence.
    Verdict,
    /// The [`classifier_baseline`].
    Baseline,
}

/// One contender's score, decomposed. The three signals sum to the total.
#[derive(Clone, Debug, Serialize)]
pub struct CandidateScore {
    pub route: String,
    pub basis: ClassifierBasis,
    /// `W_classifier · confidence`.
    pub classifier_signal: f64,
    /// `W_prior · prior`.
    pub prior_signal: f64,
    /// `W_history · history.value`.
    pub history_signal: f64,
    pub total_score: f64,
    /// The unweighted classifier input: the verdict's confidence, or the
    /// baseline.
    pub confidence: f64,
    pub prior: f64,
    pub history: HistorySignal,
}

/// The `scoring` block of a routing trace. Route names and numbers only:
/// never a deployment, node, session, prompt or request content.
#[derive(Clone, Debug, Serialize)]
pub struct ScoringTrace {
    pub enabled: bool,
    pub reason: ScoringReason,
    pub classifier_baseline: f64,
    pub influence_radius: f64,
    pub weights: Weights,
    /// The route R9.1 alone resolves the classification to.
    pub classified_route: String,
    pub winner: String,
    /// The winner is not the route R9.1 alone would have taken.
    pub overrode: bool,
    /// A verdict R9.1 rejected as below the threshold, never a contender.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejected_route: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejected_confidence: Option<f64>,
    /// The verdict route first, then the fallback. Empty when nothing
    /// contended.
    pub candidates: Vec<CandidateScore>,
}

/// The route scoring resolved a classification to, and why.
#[derive(Clone, Debug)]
pub struct ScoringDecision {
    pub route: RouteName,
    pub trace: ScoringTrace,
}

/// Resolve `classification` to one logical route.
///
/// Pure arithmetic over the classification, the configuration and `history`
/// read at `now`: no I/O, no network, no model, no deployment.
pub fn decide(
    scoring: &AdaptiveScoring,
    history: &HistoryBook,
    classification: &Classification,
    classifier: &RouteClassifier,
    now: Instant,
) -> ScoringDecision {
    let baseline = classifier_baseline(classifier);
    // R9.1's own resolution, unchanged: the input boundary scoring starts from.
    let classified = classification.route(classifier).clone();
    let mut trace = ScoringTrace {
        enabled: true,
        reason: ScoringReason::NoVerdict,
        classifier_baseline: baseline,
        influence_radius: scoring.weights.influence_radius(),
        weights: scoring.weights,
        classified_route: classified.to_string(),
        winner: classified.to_string(),
        overrode: false,
        rejected_route: None,
        rejected_confidence: None,
        candidates: Vec::new(),
    };
    let verdict = match (&classification.verdict, classification.outcome) {
        (Some(verdict), ClassifierOutcome::Chosen) => verdict,
        (Some(rejected), ClassifierOutcome::LowConfidence) => {
            // The hard boundary: R9.1 rejected this route. Its fallback is the
            // accepted decision, and the rejected route does not contend.
            trace.reason = ScoringReason::BelowThreshold;
            trace.rejected_route = Some(rejected.route.to_string());
            trace.rejected_confidence = Some(rejected.confidence);
            return ScoringDecision {
                route: classified,
                trace,
            };
        }
        _ => {
            return ScoringDecision {
                route: classified,
                trace,
            };
        }
    };

    let score = |route: &RouteName, basis: ClassifierBasis, confidence: f64| {
        let prior = scoring.prior(route);
        let signal = history.signal(route, now);
        let weights = &scoring.weights;
        let classifier_signal = weights.classifier * confidence;
        let prior_signal = weights.prior * prior;
        let history_signal = weights.history * signal.value;
        CandidateScore {
            route: route.to_string(),
            basis,
            classifier_signal,
            prior_signal,
            history_signal,
            total_score: classifier_signal + prior_signal + history_signal,
            confidence,
            prior,
            history: signal,
        }
    };
    trace.candidates.push(score(
        &verdict.route,
        ClassifierBasis::Verdict,
        verdict.confidence,
    ));
    let fallback = &classifier.fallback;
    if verdict.route == *fallback {
        trace.reason = ScoringReason::Uncontested;
        trace.winner = verdict.route.to_string();
        return ScoringDecision {
            route: verdict.route.clone(),
            trace,
        };
    }
    trace
        .candidates
        .push(score(fallback, ClassifierBasis::Baseline, baseline));

    let (verdict_score, fallback_score) = (
        trace.candidates[0].total_score,
        trace.candidates[1].total_score,
    );
    if !(verdict_score.is_finite() && fallback_score.is_finite()) {
        trace.reason = ScoringReason::InternalError;
        return ScoringDecision {
            route: classified,
            trace,
        };
    }
    // Ties go to the verdict: R9.1's `confidence ≥ min_confidence` at equality.
    let winner = if fallback_score > verdict_score {
        fallback.clone()
    } else {
        verdict.route.clone()
    };
    trace.reason = ScoringReason::Scored;
    trace.overrode = winner != classified;
    trace.winner = winner.to_string();
    ScoringDecision {
        route: winner,
        trace,
    }
}

/// Check the section against the classifier it scores for.
///
/// `classifier` is the validated classifier, when it validated; `written`
/// says whether the file has a classifier section at all, so a section that
/// failed its own checks is not reported twice.
pub(crate) fn validate(
    raw: &AdaptiveScoringFile,
    classifier: Option<&RouteClassifier>,
    written: bool,
    errors: &mut Vec<ConfigError>,
) -> Option<AdaptiveScoring> {
    let mut problems: Vec<String> = Vec::new();

    let weights = Weights {
        classifier: raw.weights.classifier,
        prior: raw.weights.prior,
        history: raw.weights.history,
    };
    let mut weights_ok = true;
    if !(weights.classifier.is_finite() && weights.classifier > 0.0 && weights.classifier <= 1.0) {
        problems.push("weights.classifier must be greater than 0 and at most 1".into());
        weights_ok = false;
    }
    for (name, value) in [("prior", weights.prior), ("history", weights.history)] {
        if !(value.is_finite() && (0.0..=1.0).contains(&value)) {
            problems.push(format!("weights.{name} must be between 0 and 1"));
            weights_ok = false;
        }
    }

    let history = &raw.history;
    if !(MIN_HALF_LIFE_SECS..=MAX_HALF_LIFE_SECS).contains(&history.half_life_secs) {
        problems.push(format!(
            "history.half_life_secs must be between {MIN_HALF_LIFE_SECS} and {MAX_HALF_LIFE_SECS}"
        ));
    }
    if !(1..=MAX_SAMPLES).contains(&history.min_samples) {
        problems.push(format!(
            "history.min_samples must be between 1 and {MAX_SAMPLES}"
        ));
    }
    let shrinkage_samples = history.shrinkage_samples.unwrap_or(history.min_samples);
    if !(1..=MAX_SAMPLES).contains(&shrinkage_samples) {
        problems.push(format!(
            "history.shrinkage_samples must be between 1 and {MAX_SAMPLES}"
        ));
    }

    let mut priors: Vec<(RouteName, f64)> = Vec::new();
    match classifier {
        None if !written => problems.push(
            "needs auto_route.classifier: scoring ranks the routes a classification leaves \
             plausible, and there is no classification without one"
                .into(),
        ),
        // The classifier's own errors are already reported.
        None => {}
        Some(classifier) => {
            // The routes that can ever contend: any candidate a verdict can
            // name, and the fallback.
            let contenders: Vec<&RouteName> = classifier
                .candidates
                .iter()
                .map(|candidate| &candidate.route)
                .chain(std::iter::once(&classifier.fallback))
                .collect();
            for (name, prior) in &raw.priors {
                if is_auto(name) {
                    problems.push(format!("priors {name:?} is Auto itself; name a route"));
                    continue;
                }
                if alias::is_reserved(name) {
                    problems.push(format!("priors {name:?} is reserved; name a route"));
                    continue;
                }
                let Some(route) = contenders.iter().find(|route| route.matches(name)) else {
                    problems.push(format!(
                        "priors {name:?} is not a classifier candidate or its fallback route, \
                         so scoring would never read it"
                    ));
                    continue;
                };
                if !(prior.is_finite() && (0.0..=1.0).contains(prior)) {
                    problems.push(format!("priors {name:?} must be between 0 and 1"));
                    continue;
                }
                if priors.iter().any(|(seen, _)| seen == *route) {
                    problems.push(format!(
                        "priors names {route} more than once (route names are compared \
                         ignoring case)"
                    ));
                    continue;
                }
                priors.push(((*route).clone(), *prior));
            }
            priors.sort_by_key(|(route, _)| contenders.iter().position(|c| *c == route));

            // The dominance bound, against every configured provider's
            // threshold, so switching provider cannot silently break it.
            if weights_ok {
                let radius = weights.influence_radius();
                for provider in
                    std::iter::once(&classifier.provider).chain(classifier.standby.as_ref())
                {
                    let baseline = provider.limits().min_confidence;
                    let limit = MAX_INFLUENCE_SHARE * (1.0 - baseline);
                    if radius > 0.0 && radius >= limit {
                        problems.push(format!(
                            "weights let priors and history move a decision by up to {radius:.3} \
                             in confidence ((prior + 2 x history) / classifier); with the {} \
                             classifier's min_confidence of {baseline}, that must be less than \
                             {limit:.3}, half of the accepted range, so a confident \
                             classification is never overturned",
                            provider.kind().as_str()
                        ));
                    }
                }
            }
        }
    }

    if !problems.is_empty() {
        errors.extend(
            problems
                .into_iter()
                .map(|problem| ConfigError::BadAdaptiveScoring { problem }),
        );
        return None;
    }
    Some(AdaptiveScoring {
        enabled: raw.enabled,
        weights,
        priors,
        history: HistoryPolicy {
            half_life: Duration::from_secs(history.half_life_secs),
            min_samples: history.min_samples,
            shrinkage_samples,
        },
    })
}

#[cfg(test)]
mod tests;
