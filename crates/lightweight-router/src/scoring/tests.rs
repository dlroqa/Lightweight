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

/// The largest weights validation accepts for `min_confidence` 0.65, a hair
/// under the bound: `(prior + 2·history) / classifier < 0.175`.
fn strongest() -> Value {
    json!({"enabled": true, "weights": {"classifier": 1.0, "prior": 0.07, "history": 0.0524}})
}

/// The worst case for the verdict: the fallback has the top prior and a
/// perfect record, the verdict route the bottom prior and a failed one.
fn stack_against_coder(setup: &Setup, now: Instant) {
    feed(setup, "General", Observation::Success, 5_000, now);
    feed(setup, "Coder", Observation::Failure, 5_000, now);
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
    // History is recorded, and must not matter while its weight is zero.
    stack_against_coder(&setup, now);
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

// --- the confidence threshold boundary -----------------------------------------

#[test]
fn a_confidence_well_above_the_threshold_is_never_overturned() {
    let setup = setup(strongest());
    let now = Instant::now();
    stack_against_coder(&setup, now);
    let radius = setup.scoring.weights.influence_radius();
    assert!(radius < 0.175 && radius > 0.17, "{radius}");
    // At and above baseline + radius, nothing moves the verdict.
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
fn a_prior_and_history_may_decide_a_verdict_exactly_at_the_threshold() {
    let setup = setup(json!({"enabled": true,
        "weights": {"prior": 0.1, "history": 0.03},
        "priors": {"General": 0.5}}));
    // Nothing else known: a tie on classifier signal, broken by General's
    // prior.
    let decision = decide_at(&setup, &classified(&setup, "Coder", 0.65), Instant::now());
    assert_eq!(decision.route.as_str(), "General");
    assert!(decision.trace.overrode);
    assert_eq!(decision.trace.reason, ScoringReason::Scored);
    assert_eq!(decision.trace.classified_route, "Coder");

    // History alone can decide it too.
    let setup = setup_history_only();
    let now = Instant::now();
    feed(&setup, "Coder", Observation::Failure, 40, now);
    assert_eq!(
        decide_at(&setup, &classified(&setup, "Coder", 0.65), now)
            .route
            .as_str(),
        "General"
    );
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

fn setup_history_only() -> Setup {
    setup(json!({"enabled": true, "weights": {"history": 0.08}}))
}

#[test]
fn a_verdict_below_the_threshold_is_rejected_and_never_resurrected() {
    // Everything favours Coder: the top prior, a perfect record, and General
    // with a failed one. R9.1 rejected Coder at 0.40, and that stands.
    let setup = setup(json!({"enabled": true,
        "weights": {"prior": 0.07, "history": 0.0524},
        "priors": {"Coder": 1.0}}));
    let now = Instant::now();
    feed(&setup, "Coder", Observation::Success, 5_000, now);
    feed(&setup, "General", Observation::Failure, 5_000, now);
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
    // Research has the top prior and a perfect record, but the classifier
    // named Coder: there is no signal for Research, and it cannot win.
    let setup = setup(json!({"enabled": true,
        "weights": {"prior": 0.07, "history": 0.0524},
        "priors": {"Research": 1.0}}));
    let now = Instant::now();
    feed(&setup, "Research", Observation::Success, 5_000, now);
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
fn property_no_valid_configuration_lets_priors_or_history_overturn_a_strong_classification() {
    let mut random = Lcg(0x5eed);
    let mut checked = 0;
    for _ in 0..400 {
        let min_confidence = 0.05 + 0.9 * random.next();
        let classifier_weight = 0.05 + 0.95 * random.next();
        // Spread over and past the bound, so both sides of it are sampled.
        let prior = 0.3 * random.next();
        let history = 0.15 * random.next();
        let scoring = json!({"enabled": true,
            "weights": {"classifier": classifier_weight, "prior": prior, "history": history},
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
        // The worst history for Coder and the best for General.
        feed(&setup, "General", Observation::Success, 2_000, now);
        feed(&setup, "Coder", Observation::Failure, 2_000, now);
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

#[test]
fn property_scoring_moves_a_decision_only_within_the_influence_radius() {
    let setup = setup(strongest());
    let now = Instant::now();
    stack_against_coder(&setup, now);
    let radius = setup.scoring.weights.influence_radius();
    let mut overrides = 0;
    for step in 0..=350 {
        let confidence = 0.65 + f64::from(step) / 1_000.0;
        let decision = decide_at(&setup, &classified(&setup, "Coder", confidence), now);
        if decision.trace.overrode {
            overrides += 1;
            assert!(
                confidence - 0.65 < radius,
                "overrode at {confidence}, outside the radius {radius}"
            );
        }
    }
    assert!(
        overrides > 100,
        "the worst case does flip the band near the threshold"
    );
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
    let setup = setup(json!({"enabled": true, "weights": {"prior": 0.1},
        "priors": {"General": 1.0}}));
    assert_eq!(winner(&setup, "Coder", 0.70), "General", "within 0.1");
    assert_eq!(
        winner(&setup, "Coder", 0.76),
        "Coder",
        "beyond the prior's reach"
    );
    // A prior for the verdict route helps it hold, too.
    let setup = setup_with_coder_prior();
    let now = Instant::now();
    feed(&setup, "Coder", Observation::Failure, 40, now);
    assert_eq!(
        decide_at(&setup, &classified(&setup, "Coder", 0.66), now)
            .route
            .as_str(),
        "Coder"
    );
}

fn setup_with_coder_prior() -> Setup {
    setup(
        json!({"enabled": true, "weights": {"prior": 0.1, "history": 0.03},
        "priors": {"Coder": 1.0}}),
    )
}

#[test]
fn a_prior_cannot_overpower_a_strong_classification() {
    let setup = setup(json!({"enabled": true, "weights": {"prior": 0.17},
        "priors": {"General": 1.0}}));
    assert_eq!(winner(&setup, "Coder", 0.83), "Coder");
    assert_eq!(winner(&setup, "Coder", 0.99), "Coder");
}

// --- history -------------------------------------------------------------------

#[test]
fn success_history_raises_a_route_within_its_cap() {
    let setup = setup_history_only();
    let now = Instant::now();
    // General's perfect record beats a borderline Coder, but no further than
    // 2 · 0.08 = 0.16 above the threshold — here only 0.08, as Coder has none.
    feed(&setup, "General", Observation::Success, 10_000, now);
    let decide = |confidence| {
        decide_at(&setup, &classified(&setup, "Coder", confidence), now)
            .route
            .to_string()
    };
    assert_eq!(decide(0.70), "General");
    assert_eq!(decide(0.73), "Coder", "beyond history's cap");
}

#[test]
fn failure_history_lowers_a_route_within_its_cap() {
    let setup = setup_history_only();
    let now = Instant::now();
    feed(&setup, "Coder", Observation::Failure, 10_000, now);
    let decision = decide_at(&setup, &classified(&setup, "Coder", 0.70), now);
    assert_eq!(decision.route.as_str(), "General");
    let coder = &decision.trace.candidates[0];
    assert!(coder.history_signal < 0.0 && coder.history_signal > -0.08);
    assert_eq!(
        decide_at(&setup, &classified(&setup, "Coder", 0.74), now)
            .route
            .as_str(),
        "Coder"
    );
}

#[test]
fn history_below_min_samples_does_not_count() {
    let setup = setup_history_only();
    let now = Instant::now();
    feed(&setup, "Coder", Observation::Failure, 19, now);
    let decision = decide_at(&setup, &classified(&setup, "Coder", 0.66), now);
    assert_eq!(decision.route.as_str(), "Coder");
    assert!(decision.trace.candidates[0].history.gated);
    assert_eq!(decision.trace.candidates[0].history_signal, 0.0);
}

#[test]
fn unavailable_and_mismatch_never_move_the_score() {
    let setup = setup_history_only();
    let now = Instant::now();
    feed(&setup, "Coder", Observation::Unavailable, 1_000, now);
    feed(&setup, "Coder", Observation::CapabilityMismatch, 1_000, now);
    feed(&setup, "Coder", Observation::Neutral, 1_000, now);
    let decision = decide_at(&setup, &classified(&setup, "Coder", 0.65), now);
    assert_eq!(
        decision.route.as_str(),
        "Coder",
        "an unready route is not a bad route"
    );
    assert_eq!(decision.trace.candidates[0].history, HistorySignal::NEUTRAL);
}

#[test]
fn history_fades_and_a_decision_recovers_without_exploration() {
    let setup = setup_history_only();
    let start = Instant::now();
    feed(&setup, "Coder", Observation::Failure, 80, start);
    let decide = |at| {
        decide_at(&setup, &classified(&setup, "Coder", 0.66), at)
            .route
            .to_string()
    };
    assert_eq!(decide(start), "General");
    assert_eq!(
        decide(start + Duration::from_secs(3 * 3_600)),
        "Coder",
        "below min_samples after three half-lives: neutral again"
    );
}

// --- the trace ----------------------------------------------------------------

#[test]
fn the_trace_explains_the_decision_and_names_no_deployment() {
    let setup = setup(json!({"enabled": true,
        "weights": {"prior": 0.1, "history": 0.03},
        "priors": {"General": 0.5}}));
    let now = Instant::now();
    feed(&setup, "General", Observation::Success, 30, now);
    let decision = decide_at(&setup, &classified(&setup, "Coder", 0.70), now);
    let trace = &decision.trace;
    assert_eq!(trace.classifier_baseline, 0.65);
    for candidate in &trace.candidates {
        let sum = candidate.classifier_signal + candidate.prior_signal + candidate.history_signal;
        assert!((sum - candidate.total_score).abs() < 1e-12, "{candidate:?}");
    }
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
    let setup = setup(strongest());
    let now = Instant::now();
    stack_against_coder(&setup, now);
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
        "weights.history must be between 0 and 1",
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
    let found = errors(json!({"enabled": true, "weights": {"prior": 0.1, "history": 0.05}}));
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].contains("move a decision by up to 0.200") && found[0].contains("less than 0.175"),
        "{found:?}"
    );
    // A smaller classifier weight magnifies what priors and history can do.
    let found = errors(json!({"weights": {"classifier": 0.5, "prior": 0.09, "history": 0.0}}));
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
fn a_threshold_of_one_leaves_no_room_for_priors_or_history() {
    let mut auto = auto_section(json!({"weights": {"history": 0.001}}));
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
