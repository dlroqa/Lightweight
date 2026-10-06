use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::*;
use crate::classifier::{ProviderKind, Verdict};
use crate::config::{RouterConfig, RouterFile};

fn file(auto: Value) -> RouterFile {
    serde_json::from_value(json!({
        "nodes": [{"id": "a", "url": "http://192.0.2.10:11434"}],
        "routes": [
            {"name": "General", "deployments": [{"node": "a", "model": "G"}]},
            {"name": "Coder", "deployments": [{"node": "a", "model": "C"}]},
            {"name": "Research", "deployments": [{"node": "a", "model": "R"}]},
            {"name": "ToolAgent", "deployments": [{"node": "a", "model": "T"}]},
            {"name": "RouterClassifier", "deployments": [{"node": "a", "model": "Q"}]}
        ],
        "auto_route": auto,
    }))
    .expect("the file shape parses")
}

fn env(name: &str) -> Option<String> {
    (name == "TYPESAFE_API_KEY").then(|| "test-key".to_owned())
}

fn auto_section(scoring: Value) -> Value {
    json!({
        "enabled": true,
        "fallback_route": "General",
        "classifier": {
            "routes": ["General", "Coder", "Research"],
            "lightweight": {"route": "RouterClassifier", "timeout_ms": 30_000,
                            "min_confidence": 0.65}
        },
        "adaptive_scoring": scoring,
        "rules": [
            {"name": "forced-tools", "when": {"tool_choice": "required"}, "route": "ToolAgent"},
            {"name": "semantic", "when": {}, "classify": true}
        ]
    })
}

fn config(scoring: Value) -> RouterConfig {
    crate::config::validate(file(auto_section(scoring)), &env).expect("valid")
}

fn errors_of(auto: Value) -> Vec<String> {
    crate::config::validate(file(auto), &env)
        .expect_err("refused")
        .0
        .iter()
        .map(ToString::to_string)
        .collect()
}

fn errors(scoring: Value) -> Vec<String> {
    errors_of(auto_section(scoring))
}

/// The classifier, the scoring section, and an empty recording history book.
struct Setup {
    classifier: RouteClassifier,
    scoring: AdaptiveScoring,
    history: HistoryBook,
}

fn setup(scoring: Value) -> Setup {
    let config = config(scoring);
    let auto = config.auto.expect("auto");
    let scoring = auto.scoring.expect("scoring");
    let history = HistoryBook::new(
        config
            .topology
            .routes()
            .iter()
            .map(|route| route.name.clone())
            .collect(),
        Some(scoring.history),
    );
    Setup {
        classifier: auto.classifier.expect("classifier"),
        scoring,
        history,
    }
}

fn name(raw: &str) -> RouteName {
    RouteName::parse(raw).unwrap()
}

/// A classification as the classifier makes it: `chosen` at or above the
/// threshold, `low_confidence` below it.
fn classified(setup: &Setup, route: &str, confidence: f64) -> Classification {
    let outcome = if confidence < setup.classifier.limits().min_confidence {
        ClassifierOutcome::LowConfidence
    } else {
        ClassifierOutcome::Chosen
    };
    Classification {
        provider: ProviderKind::Lightweight,
        outcome,
        verdict: Some(Verdict {
            route: name(route),
            confidence,
            model: None,
        }),
        duration: Duration::ZERO,
        request_id: String::new(),
        input_truncated: false,
    }
}

fn failed(outcome: ClassifierOutcome) -> Classification {
    Classification {
        provider: ProviderKind::Jev,
        outcome,
        verdict: None,
        duration: Duration::ZERO,
        request_id: String::new(),
        input_truncated: false,
    }
}

fn decide_at(setup: &Setup, classification: &Classification, now: Instant) -> ScoringDecision {
    decide(
        &setup.scoring,
        &setup.history,
        classification,
        &setup.classifier,
        now,
    )
}

fn winner(setup: &Setup, route: &str, confidence: f64) -> String {
    decide_at(setup, &classified(setup, route, confidence), Instant::now())
        .route
        .to_string()
}

/// `n` observations of `route` at `now`.
fn feed(setup: &Setup, route: &str, observation: Observation, n: u32, now: Instant) {
    for _ in 0..n {
        assert!(setup.history.observe(&name(route), observation, now));
    }
}

/// The largest prior weight validation accepts for `min_confidence` 0.65, a
/// hair under the bound: `prior / classifier < 0.175`.
fn strongest() -> Value {
    json!({"enabled": true, "weights": {"classifier": 1.0, "prior": 0.17}})
}

/// `strongest()` with every prior on the fallback: the most the active inputs
/// can do against a Coder verdict.
fn strongest_against_coder() -> Value {
    json!({"enabled": true, "weights": {"classifier": 1.0, "prior": 0.17},
           "priors": {"General": 1.0}})
}

/// A borderline setup: General's prior is worth 0.10, so Coder verdicts in
/// [0.65, 0.75) lose to it and those at 0.75 and above win.
fn borderline() -> Value {
    json!({"enabled": true, "weights": {"prior": 0.1}, "priors": {"General": 1.0}})
}

/// Massive popularity for one route, nothing for the other.
fn popular(setup: &Setup, route: &str, now: Instant) {
    feed(setup, route, Observation::Success, 10_000, now);
}

/// Confidences around and across the borderline, for comparing decisions.
fn sweep() -> impl Iterator<Item = f64> {
    (0..=350).map(|step| 0.65 + f64::from(step) / 1_000.0)
}

/// `(winner, verdict total, fallback total)` for a Coder verdict at
/// `confidence`.
fn outcome(setup: &Setup, confidence: f64, now: Instant) -> (String, f64, f64) {
    let decision = decide_at(setup, &classified(setup, "Coder", confidence), now);
    (
        decision.route.to_string(),
        decision.trace.candidates[0].total_score,
        decision.trace.candidates[1].total_score,
    )
}

// --- disabled / compatibility --------------------------------------------------

/// Every classification shape, across a sweep of confidences around the
/// threshold.
fn every_classification(setup: &Setup) -> Vec<Classification> {
    let mut all: Vec<Classification> = (0..=100)
        .flat_map(|step| {
            let confidence = f64::from(step) / 100.0;
            ["General", "Coder", "Research"]
                .into_iter()
                .map(move |route| (route, confidence))
        })
        .map(|(route, confidence)| classified(setup, route, confidence))
        .collect();
    for outcome in [
        ClassifierOutcome::Invalid,
        ClassifierOutcome::Unavailable,
        ClassifierOutcome::Timeout,
        ClassifierOutcome::AuthError,
        ClassifierOutcome::RateLimited,
        ClassifierOutcome::ConnectionError,
        ClassifierOutcome::ProviderError,
        ClassifierOutcome::Nested,
    ] {
        all.push(failed(outcome));
    }
    all
}

#[test]
fn neutral_weights_reproduce_r91_exactly_whatever_the_history() {
    let setup = setup(json!({"enabled": true}));
    let now = Instant::now();
    popular(&setup, "General", now);
    feed(&setup, "Coder", Observation::ServerError, 5_000, now);
    for classification in every_classification(&setup) {
        let decision = decide_at(&setup, &classification, now);
        assert_eq!(
            &decision.route,
            classification.route(&setup.classifier),
            "{classification:?}"
        );
        assert!(!decision.trace.overrode);
    }
}

#[test]
fn defaults_are_neutral_and_documented() {
    let setup = setup(json!({}));
    assert!(!setup.scoring.enabled, "off unless turned on");
    assert_eq!(
        setup.scoring.weights,
        Weights {
            classifier: 1.0,
            prior: 0.0,
            history: 0.0
        }
    );
    assert_eq!(setup.scoring.weights.influence_radius(), 0.0);
    assert_eq!(
        setup.scoring.history,
        HistoryPolicy {
            half_life: Duration::from_secs(3_600),
            min_samples: 20,
            shrinkage_samples: 20,
        },
        "a one-hour half-life, 20 samples, shrinkage of 20"
    );
    assert!(setup.scoring.priors.is_empty());
}

#[test]
fn an_absent_section_configures_nothing() {
    let mut auto = auto_section(json!({}));
    auto.as_object_mut().unwrap().remove("adaptive_scoring");
    let config = crate::config::validate(file(auto), &env).unwrap();
    assert!(config.auto.unwrap().scoring.is_none());
}

// --- history_weight is 0 in slice 1 -------------------------------------------

#[test]
fn a_zero_history_weight_is_valid() {
    let setup = setup(json!({"enabled": true,
        "weights": {"classifier": 1.0, "prior": 0.1, "history": 0.0}}));
    assert_eq!(setup.scoring.weights.history, 0.0);
    // Omitted means 0, too.
    assert_eq!(
        setup_of(json!({"weights": {"prior": 0.1}}))
            .scoring
            .weights
            .history,
        0.0
    );
}

/// [`setup`], callable where a local `setup` shadows it.
fn setup_of(scoring: Value) -> Setup {
    setup(scoring)
}

#[test]
fn a_nonzero_history_weight_is_refused_with_its_reason() {
    for weight in [0.0001, 0.01, 0.06, 1.0] {
        let found = errors(json!({"enabled": true,
            "weights": {"classifier": 1.0, "prior": 0.1, "history": weight}}));
        assert_eq!(found.len(), 1, "{weight}: {found:?}");
        let message = &found[0];
        for expected in [
            "auto_route.adaptive_scoring: weights.history must be 0",
            "observational only",
            "R9.2 slice 1",
            "successful traffic volume, not route-attributable quality",
        ] {
            assert!(message.contains(expected), "{expected}: {message}");
        }
    }
    // Refused while off too, and negative or non-finite values as well.
    assert_eq!(
        errors(json!({"enabled": false, "weights": {"history": 0.5}})).len(),
        1
    );
    assert!(
        errors(json!({"weights": {"history": -0.01}}))[0].contains("weights.history must be 0")
    );
}

// --- the active influence radius -----------------------------------------------

#[test]
fn the_influence_radius_counts_only_the_active_prior() {
    for (classifier, prior) in [(1.0, 0.1), (0.5, 0.08), (0.8, 0.0), (1.0, 0.17)] {
        let setup = setup(json!({"weights": {"classifier": classifier, "prior": prior,
                                             "history": 0.0}}));
        let radius = setup.scoring.weights.influence_radius();
        assert!(
            (radius - prior / classifier).abs() < 1e-12,
            "{classifier}/{prior}: {radius}"
        );
    }
    // No phantom history allowance: a prior of 0.17 is within 0.175, which
    // `(prior + 2·history)` with any history term would not have needed to be.
    let setup = setup(strongest_against_coder());
    assert!((setup.scoring.weights.influence_radius() - 0.17).abs() < 1e-12);
    let decision = decide_at(&setup, &classified(&setup, "Coder", 0.7), Instant::now());
    assert!((decision.trace.influence_radius - 0.17).abs() < 1e-12);
}

#[test]
fn scoring_moves_a_decision_by_exactly_the_active_radius() {
    let setup = setup(strongest_against_coder());
    let now = Instant::now();
    popular(&setup, "General", now);
    let radius = setup.scoring.weights.influence_radius();
    for confidence in sweep() {
        let decision = decide_at(&setup, &classified(&setup, "Coder", confidence), now);
        // The fallback wins exactly below baseline + radius, whatever its
        // popularity.
        let expected = if confidence < 0.65 + radius - 1e-9 {
            "General"
        } else {
            "Coder"
        };
        if (confidence - (0.65 + radius)).abs() > 1e-9 {
            assert_eq!(decision.route.as_str(), expected, "{confidence}");
        }
    }
}

// --- the confidence threshold boundary -----------------------------------------

#[test]
fn a_confidence_well_above_the_threshold_is_never_overturned() {
    let setup = setup(strongest_against_coder());
    let now = Instant::now();
    popular(&setup, "General", now);
    let radius = setup.scoring.weights.influence_radius();
    assert!(radius < 0.175 && radius > 0.17 - 1e-9, "{radius}");
    let mut confidence = 0.65 + radius + 1e-9;
    while confidence <= 1.0 {
        assert_eq!(
            decide_at(&setup, &classified(&setup, "Coder", confidence), now)
                .route
                .as_str(),
            "Coder",
            "{confidence}"
        );
        confidence += 0.001;
    }
}

#[test]
fn a_bounded_prior_decides_a_verdict_exactly_at_the_threshold() {
    let setup = setup(json!({"enabled": true, "weights": {"prior": 0.1},
        "priors": {"General": 0.5}}));
    let decision = decide_at(&setup, &classified(&setup, "Coder", 0.65), Instant::now());
    assert_eq!(decision.route.as_str(), "General");
    assert!(decision.trace.overrode);
    assert_eq!(decision.trace.reason, ScoringReason::Scored);
    assert_eq!(decision.trace.classified_route, "Coder");
}

#[test]
fn history_cannot_change_an_exact_threshold_decision() {
    // No priors: an exact tie on the baseline, which the verdict wins, however
    // popular the fallback is.
    let setup = setup(strongest());
    let now = Instant::now();
    popular(&setup, "General", now);
    assert_eq!(outcome(&setup, 0.65, now).0, "Coder");
    // With a prior for General: General, however popular Coder is.
    let setup = setup_of(json!({"enabled": true, "weights": {"prior": 0.1},
        "priors": {"General": 0.5}}));
    popular(&setup, "Coder", now);
    assert_eq!(outcome(&setup, 0.65, now).0, "General");
}

#[test]
fn history_cannot_break_a_tie() {
    // Same classifier contribution (verdict at the baseline), same prior, and
    // wildly different observed history: the existing tie rule decides.
    let tie = json!({"enabled": true, "weights": {"prior": 0.1},
        "priors": {"General": 0.5, "Coder": 0.5}});
    for popular_route in ["General", "Coder"] {
        let setup = setup(tie.clone());
        let now = Instant::now();
        popular(&setup, popular_route, now);
        let (winner, verdict, fallback) = outcome(&setup, 0.65, now);
        assert_eq!(verdict, fallback, "an exact tie");
        assert_eq!(
            winner, "Coder",
            "ties go to the verdict ({popular_route} popular)"
        );
    }
}

#[test]
fn at_the_threshold_with_nothing_against_it_the_verdict_wins_the_tie() {
    let setup = setup(strongest());
    let decision = decide_at(&setup, &classified(&setup, "Coder", 0.65), Instant::now());
    assert_eq!(decision.route.as_str(), "Coder");
    assert_eq!(
        decision.trace.candidates[0].total_score, decision.trace.candidates[1].total_score,
        "an exact tie"
    );
}

#[test]
fn a_verdict_below_the_threshold_is_rejected_and_never_resurrected() {
    // Everything favours Coder: the maximum allowed prior and 10 000
    // successes, against a General with neither. R9.1 rejected Coder, and
    // that stands.
    let setup = setup(json!({"enabled": true, "weights": {"prior": 0.17},
        "priors": {"Coder": 1.0}}));
    let now = Instant::now();
    popular(&setup, "Coder", now);
    feed(&setup, "General", Observation::ServerError, 5_000, now);
    for confidence in [0.0, 0.10, 0.40, 0.60, 0.649_999_999] {
        let classification = classified(&setup, "Coder", confidence);
        assert_eq!(classification.outcome, ClassifierOutcome::LowConfidence);
        let decision = decide_at(&setup, &classification, now);
        assert_eq!(decision.route.as_str(), "General", "{confidence}");
        assert_eq!(decision.trace.reason, ScoringReason::BelowThreshold);
        assert_eq!(decision.trace.rejected_route.as_deref(), Some("Coder"));
        assert_eq!(decision.trace.rejected_confidence, Some(confidence));
        assert!(
            decision.trace.candidates.is_empty(),
            "a rejected route is not a contender"
        );
        assert!(!decision.trace.overrode, "R9.1's fallback, unchanged");
    }
}

#[test]
fn a_classification_without_a_verdict_keeps_the_r91_fallback() {
    let setup = setup(strongest());
    for outcome in [
        ClassifierOutcome::Timeout,
        ClassifierOutcome::AuthError,
        ClassifierOutcome::Invalid,
        ClassifierOutcome::Nested,
    ] {
        let decision = decide_at(&setup, &failed(outcome), Instant::now());
        assert_eq!(decision.route.as_str(), "General");
        assert_eq!(decision.trace.reason, ScoringReason::NoVerdict);
        assert!(decision.trace.candidates.is_empty());
    }
}

#[test]
fn a_verdict_for_the_fallback_itself_is_uncontested() {
    let setup = setup(strongest());
    let decision = decide_at(&setup, &classified(&setup, "General", 0.9), Instant::now());
    assert_eq!(decision.route.as_str(), "General");
    assert_eq!(decision.trace.reason, ScoringReason::Uncontested);
    assert_eq!(decision.trace.candidates.len(), 1);
}

#[test]
fn only_the_verdict_and_the_fallback_contend() {
    // Research has the top prior and 10 000 successes, but the classifier
    // named Coder: there is no signal for Research, and it cannot win.
    let setup = setup(json!({"enabled": true, "weights": {"prior": 0.17},
        "priors": {"Research": 1.0}}));
    let now = Instant::now();
    popular(&setup, "Research", now);
    for confidence in [0.65, 0.7, 0.99] {
        let decision = decide_at(&setup, &classified(&setup, "Coder", confidence), now);
        let routes: Vec<&str> = decision
            .trace
            .candidates
            .iter()
            .map(|c| c.route.as_str())
            .collect();
        assert_eq!(routes, ["Coder", "General"]);
        assert_ne!(decision.route.as_str(), "Research");
    }
}

#[test]
fn a_score_that_is_not_finite_keeps_the_r91_route() {
    let setup = setup(strongest());
    let mut broken = classified(&setup, "Coder", 0.9);
    broken.verdict.as_mut().unwrap().confidence = f64::NAN;
    let decision = decide_at(&setup, &broken, Instant::now());
    assert_eq!(decision.trace.reason, ScoringReason::InternalError);
    assert_eq!(decision.route.as_str(), "Coder", "R9.1's route stands");
}

/// A deterministic generator for the property sweeps: no new dependency, the
/// same cases on every run.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        f64::from(u32::try_from(self.0 >> 40).unwrap()) / f64::from(1_u32 << 24)
    }
}

#[test]
fn property_no_valid_configuration_lets_a_prior_overturn_a_strong_classification() {
    let mut random = Lcg(0x5eed);
    let mut checked = 0;
    for _ in 0..400 {
        let min_confidence = 0.05 + 0.9 * random.next();
        let classifier_weight = 0.05 + 0.95 * random.next();
        // Spread over and past the bound, so both sides of it are sampled.
        let prior = 0.4 * random.next();
        let scoring = json!({"enabled": true,
            "weights": {"classifier": classifier_weight, "prior": prior},
            "priors": {"General": random.next(), "Coder": random.next()}});
        let mut auto = auto_section(scoring);
        auto["classifier"]["lightweight"]["min_confidence"] = json!(min_confidence);
        let Ok(config) = crate::config::validate(file(auto), &env) else {
            // Refused by the bound: that is the property working, not a case.
            continue;
        };
        let auto = config.auto.unwrap();
        let setup = Setup {
            history: HistoryBook::new(
                vec![name("General"), name("Coder")],
                Some(auto.scoring.as_ref().unwrap().history),
            ),
            classifier: auto.classifier.unwrap(),
            scoring: auto.scoring.unwrap(),
        };
        let now = Instant::now();
        feed(&setup, "General", Observation::Success, 2_000, now);
        let radius = setup.scoring.weights.influence_radius();
        assert!(radius < 0.5 * (1.0 - min_confidence) || radius == 0.0);
        // Anywhere in the upper half of the accepted range.
        let strong = min_confidence + (0.5 + 0.5 * random.next()) * (1.0 - min_confidence);
        let decision = decide_at(&setup, &classified(&setup, "Coder", strong), now);
        assert_eq!(
            decision.route.as_str(),
            "Coder",
            "min_confidence {min_confidence}, weights {:?}, confidence {strong}",
            setup.scoring.weights
        );
        // And a rejected verdict is never resurrected.
        let weak = min_confidence * random.next() * 0.999;
        let decision = decide_at(&setup, &classified(&setup, "Coder", weak), now);
        assert_eq!(decision.route.as_str(), "General");
        checked += 1;
    }
    assert!(checked > 50, "too few valid configurations: {checked}");
}

// --- priors --------------------------------------------------------------------

#[test]
fn a_route_without_a_prior_is_neutral() {
    let setup = setup(json!({"enabled": true, "weights": {"prior": 0.1},
        "priors": {"Research": 0.5}}));
    assert_eq!(setup.scoring.prior(&name("General")), 0.0);
    assert_eq!(setup.scoring.prior(&name("Coder")), 0.0);
    assert_eq!(winner(&setup, "Coder", 0.66), "Coder");
}

#[test]
fn a_bounded_prior_changes_a_borderline_result() {
    let setup = setup(borderline());
    assert_eq!(winner(&setup, "Coder", 0.70), "General", "within 0.1");
    assert_eq!(
        winner(&setup, "Coder", 0.76),
        "Coder",
        "beyond the prior's reach"
    );
    // A prior for the verdict route helps it hold.
    let setup = setup_of(json!({"enabled": true, "weights": {"prior": 0.1},
        "priors": {"Coder": 1.0, "General": 0.5}}));
    assert_eq!(winner(&setup, "Coder", 0.66), "Coder");
}

#[test]
fn a_prior_cannot_overpower_a_strong_classification() {
    let setup = setup(strongest_against_coder());
    assert_eq!(winner(&setup, "Coder", 0.83), "Coder");
    assert_eq!(winner(&setup, "Coder", 0.99), "Coder");
}

// --- history is observational only ---------------------------------------------

#[test]
fn equal_successful_volume_cannot_affect_the_winner() {
    let quiet = setup(borderline());
    let busy = setup(borderline());
    let now = Instant::now();
    feed(&busy, "General", Observation::Success, 10, now);
    feed(&busy, "Coder", Observation::Success, 10, now);
    for confidence in sweep() {
        assert_eq!(
            outcome(&busy, confidence, now),
            outcome(&quiet, confidence, now),
            "{confidence}"
        );
    }
    assert_eq!(outcome(&busy, 0.70, now).0, "General", "the prior decides");
    assert_eq!(
        outcome(&busy, 0.80, now).0,
        "Coder",
        "the classifier decides"
    );
}

#[test]
fn ten_against_ten_thousand_successes_cannot_affect_the_winner() {
    let now = Instant::now();
    // Coder: 10 successes. General: 10 000.
    let lopsided = setup(borderline());
    feed(&lopsided, "Coder", Observation::Success, 10, now);
    popular(&lopsided, "General", now);
    // And the other way round.
    let reversed = setup(borderline());
    popular(&reversed, "Coder", now);
    feed(&reversed, "General", Observation::Success, 10, now);
    let quiet = setup(borderline());

    // Active scoring says Coder (0.80 against 0.65 + 0.10): Coder wins,
    // despite General's 10 000.
    assert_eq!(outcome(&lopsided, 0.80, now).0, "Coder");
    // Reverse the active evidence (0.70 against 0.75): General wins — because
    // classifier + prior say so, whichever route is popular.
    assert_eq!(outcome(&lopsided, 0.70, now).0, "General");
    assert_eq!(outcome(&reversed, 0.70, now).0, "General");
    assert_eq!(outcome(&reversed, 0.80, now).0, "Coder");

    // Identical decisions and identical scores, request for request.
    for confidence in sweep() {
        let expected = outcome(&quiet, confidence, now);
        assert_eq!(
            outcome(&lopsided, confidence, now),
            expected,
            "{confidence}"
        );
        assert_eq!(
            outcome(&reversed, confidence, now),
            expected,
            "{confidence}"
        );
    }

    // History is still collected, and still visible.
    let view = lopsided.history.view(now);
    assert!((view[0].observations.successes - 10_000.0).abs() < 1e-9);
    assert!((view[1].observations.successes - 10.0).abs() < 1e-9);
    assert!(view[0].observations.min_samples_reached);
    assert!(!view[1].observations.min_samples_reached);
}

#[test]
fn history_decays_and_decay_never_changes_the_winner() {
    let setup = setup(borderline());
    let start = Instant::now();
    popular(&setup, "General", start);
    let later = start + Duration::from_secs(3_600);
    let observed = setup.history.observations(&name("General"), later);
    assert!(
        (observed.effective_samples - 5_000.0).abs() < 1e-6,
        "halved after the one-hour default half-life: {observed:?}"
    );
    for confidence in sweep() {
        assert_eq!(
            outcome(&setup, confidence, start),
            outcome(&setup, confidence, later),
            "{confidence}"
        );
    }
}

#[test]
fn no_observed_outcome_moves_the_score() {
    let quiet = setup(borderline());
    let noisy = setup(borderline());
    let now = Instant::now();
    for observation in [
        Observation::Success,
        Observation::ServerError,
        Observation::Interrupted,
        Observation::Unavailable,
        Observation::CapabilityMismatch,
        Observation::Neutral,
    ] {
        feed(&noisy, "Coder", observation, 1_000, now);
        feed(&noisy, "General", observation, 3_000, now);
    }
    for confidence in sweep() {
        assert_eq!(
            outcome(&noisy, confidence, now),
            outcome(&quiet, confidence, now),
            "{confidence}"
        );
    }
    let coder = &noisy.history.view(now)[1];
    assert_eq!(
        (
            coder.server_error,
            coder.interrupted,
            coder.unavailable,
            coder.mismatch,
            coder.neutral
        ),
        (1_000, 1_000, 1_000, 1_000, 1_000),
        "every outcome is still observed"
    );
}

// --- the trace ----------------------------------------------------------------

#[test]
fn the_trace_shows_history_as_inactive_and_still_observed() {
    let setup = setup(borderline());
    let now = Instant::now();
    popular(&setup, "General", now);
    let decision = decide_at(&setup, &classified(&setup, "Coder", 0.70), now);
    let trace = &decision.trace;
    assert!(!trace.history_active);
    assert_eq!(trace.weights.history, 0.0);
    for candidate in &trace.candidates {
        assert_eq!(candidate.history_signal, 0.0, "{candidate:?}");
        let sum = candidate.classifier_signal + candidate.prior_signal;
        assert!(
            (sum - candidate.total_score).abs() < 1e-12,
            "only classifier and prior: {candidate:?}"
        );
    }
    // The fallback's popularity is reported, not used.
    let general = &trace.candidates[1];
    assert!((general.history_observations.successes - 10_000.0).abs() < 1e-9);
    assert!((general.total_score - 0.75).abs() < 1e-12);

    let text = serde_json::to_string(trace).unwrap();
    for field in [
        "\"history_active\":false",
        "\"history_signal\":0.0",
        "\"history_observations\"",
    ] {
        assert!(text.contains(field), "{field}: {text}");
    }
    for absent in ["\"value\"", "success_rate", "\"gated\""] {
        assert!(
            !text.contains(absent),
            "no quality-looking field: {absent}: {text}"
        );
    }
}

#[test]
fn the_trace_explains_the_decision_and_names_no_deployment() {
    let setup = setup(json!({"enabled": true, "weights": {"prior": 0.1},
        "priors": {"General": 0.5}}));
    let now = Instant::now();
    feed(&setup, "General", Observation::Success, 30, now);
    let decision = decide_at(&setup, &classified(&setup, "Coder", 0.70), now);
    let trace = &decision.trace;
    assert_eq!(trace.classifier_baseline, 0.65);
    assert_eq!(trace.candidates[0].basis, ClassifierBasis::Verdict);
    assert_eq!(trace.candidates[1].basis, ClassifierBasis::Baseline);
    assert_eq!(trace.candidates[1].confidence, 0.65, "the baseline");

    let text = serde_json::to_string(trace).unwrap();
    for field in [
        "\"reason\":\"scored\"",
        "\"classifier_baseline\":0.65",
        "\"winner\"",
    ] {
        assert!(text.contains(field), "{field}: {text}");
    }
    for absent in ["deployment", "node", "session", "request_id", "192.0.2.10"] {
        assert!(!text.contains(absent), "{absent}: {text}");
    }
}

#[test]
fn deciding_is_cheap() {
    let setup = setup(strongest_against_coder());
    let now = Instant::now();
    popular(&setup, "General", now);
    let classification = classified(&setup, "Coder", 0.7);
    let started = Instant::now();
    for _ in 0..10_000 {
        std::hint::black_box(decide_at(
            &setup,
            std::hint::black_box(&classification),
            now,
        ));
    }
    // Arithmetic, a lock and a small trace: a generous bound for a debug
    // build on a slow machine.
    assert!(started.elapsed() < Duration::from_millis(1_000));
}

// --- configuration ------------------------------------------------------------

#[test]
fn weights_and_history_settings_are_bounded() {
    let found = errors(json!({"enabled": true,
        "weights": {"classifier": 0.0, "prior": -0.1, "history": 1.5},
        "history": {"half_life_secs": 59, "min_samples": 0, "shrinkage_samples": 10_001}}));
    for expected in [
        "weights.classifier must be greater than 0 and at most 1",
        "weights.prior must be between 0 and 1",
        "weights.history must be 0",
        "history.half_life_secs must be between 60 and 604800",
        "history.min_samples must be between 1 and 10000",
        "history.shrinkage_samples must be between 1 and 10000",
    ] {
        assert!(
            found.iter().any(|e| e.contains(expected)),
            "{expected}: {found:?}"
        );
    }
    assert!(
        found
            .iter()
            .all(|e| e.starts_with("auto_route.adaptive_scoring:")),
        "{found:?}"
    );
    let found = errors(json!({"weights": {"classifier": 1.5}}));
    assert!(found[0].contains("at most 1"), "{found:?}");
    let found = errors(json!({"history": {"half_life_secs": 604_801}}));
    assert!(found[0].contains("half_life_secs"), "{found:?}");
}

#[test]
fn non_finite_numbers_never_parse() {
    for scoring in [
        r#"{"weights": {"prior": NaN}}"#,
        r#"{"weights": {"prior": Infinity}}"#,
        r#"{"priors": {"General": 1e999}}"#,
    ] {
        let parsed: Result<AdaptiveScoringFile, _> = serde_json::from_str(scoring);
        assert!(parsed.is_err(), "{scoring}");
    }
}

#[test]
fn the_dominance_bound_is_enforced_against_every_configured_provider() {
    let found = errors(json!({"enabled": true, "weights": {"prior": 0.2}}));
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].contains("move a decision by up to 0.200 in confidence (prior / classifier)")
            && found[0].contains("less than 0.175"),
        "{found:?}"
    );
    // A smaller classifier weight magnifies what priors can do.
    let found = errors(json!({"weights": {"classifier": 0.5, "prior": 0.09}}));
    assert!(found[0].contains("0.180"), "{found:?}");
    let _ = config(json!({"weights": {"classifier": 0.5, "prior": 0.08}}));

    // A standby Jev block with a stricter threshold is checked too, so
    // switching provider cannot break the guarantee.
    let mut auto = auto_section(json!({"weights": {"prior": 0.1}}));
    auto["classifier"]["jev"] =
        json!({"model": "jev-latest", "timeout_ms": 5_000, "min_confidence": 0.9});
    let found = errors_of(auto);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].contains("the jev classifier's min_confidence of 0.9"),
        "{found:?}"
    );
}

#[test]
fn a_threshold_of_one_leaves_no_room_for_priors() {
    let mut auto = auto_section(json!({"weights": {"prior": 0.001}}));
    auto["classifier"]["lightweight"]["min_confidence"] = json!(1.0);
    assert_eq!(errors_of(auto).len(), 1);
    let mut auto = auto_section(json!({"enabled": true}));
    auto["classifier"]["lightweight"]["min_confidence"] = json!(1.0);
    assert!(crate::config::validate(file(auto), &env).is_ok());
}

#[test]
fn priors_name_only_scoring_candidates_once() {
    let found = errors(json!({"priors": {
        "Auto": 0.1, "default": 0.1, "ToolAgent": 0.1, "Nowhere": 0.1,
        "Coder": 0.1, "coder": 0.2, "Research": 1.5
    }}));
    for expected in [
        "priors \"Auto\" is Auto itself",
        "priors \"default\" is reserved",
        "priors \"ToolAgent\" is not a classifier candidate or its fallback route",
        "priors \"Nowhere\" is not a classifier candidate",
        "priors names Coder more than once",
        "priors \"Research\" must be between 0 and 1",
    ] {
        assert!(
            found.iter().any(|e| e.contains(expected)),
            "{expected}: {found:?}"
        );
    }
    let setup = setup(json!({"priors": {"research": 0.3, "GENERAL": 0.2}}));
    assert_eq!(
        setup.scoring.priors,
        vec![(name("General"), 0.2), (name("Research"), 0.3)],
        "stored as the routes are spelled, in candidate order"
    );
}

#[test]
fn a_fallback_outside_the_candidates_may_have_a_prior() {
    let mut auto = auto_section(json!({"priors": {"ToolAgent": 0.2}}));
    auto["classifier"]["fallback_route"] = json!("ToolAgent");
    let config = crate::config::validate(file(auto), &env).unwrap();
    assert_eq!(
        config
            .auto
            .unwrap()
            .scoring
            .unwrap()
            .prior(&name("ToolAgent")),
        0.2
    );
}

#[test]
fn scoring_needs_a_classifier() {
    let found = errors_of(json!({"enabled": true, "fallback_route": "General",
        "adaptive_scoring": {"enabled": true},
        "rules": [{"name": "tools", "when": {"requires_tools": true}, "route": "Coder"}]}));
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].contains("needs auto_route.classifier"),
        "{found:?}"
    );
}

#[test]
fn unknown_keys_are_refused_and_no_provider_weights_exist() {
    for scoring in [
        json!({"enabled": true, "latency_weight": 0.1}),
        json!({"weights": {"jev": 0.5}}),
        json!({"weights": {"jev_classifier": 0.5}}),
        json!({"weights": {"lightweight_classifier": 0.5}}),
        json!({"weights": {"success": 0.1}}),
        json!({"history": {"window": 100}}),
        json!({"history": {"persist": true}}),
        json!({"exploration": 0.1}),
    ] {
        let parsed: Result<RouterFile, _> = serde_json::from_value(json!({
            "nodes": [], "routes": [],
            "auto_route": {"fallback_route": "General", "adaptive_scoring": scoring}
        }));
        assert!(parsed.is_err(), "{scoring}");
    }
}

#[test]
fn a_section_that_is_off_is_still_checked() {
    let found = errors(json!({"enabled": false, "weights": {"prior": 2.0}}));
    assert!(
        found.iter().any(|e| e.contains("weights.prior")),
        "{found:?}"
    );
}
