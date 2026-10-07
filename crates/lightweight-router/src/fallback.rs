//! Explicit cross-route fallback (R9.3.1): when the logical route an `Auto`
//! request resolved to **cannot execute** it, try the next logical route the
//! operator listed for that route — before anything was committed.
//!
//! It is recovery from a route's inability to execute, and nothing else: not
//! reclassification, not re-scoring, not deployment failover, not a quality
//! retry, not mixture-of-agents.
//!
//! The rules, each enforced by construction and covered by a test:
//!
//! * **`Auto` only.** Only a request whose original `model` was `Auto` is
//!   eligible, whichever way `Auto` chose its route (an R8 rule, the R9.1
//!   classifier, R9.2 scoring or `fallback_route`). A client that named a
//!   route gets that route or its error.
//! * **Same-route failover first.** A route fails only once the existing
//!   pipeline — health, R5, affinity, policy, deployment failover — has
//!   finished with one of the three [`FallbackReason`]s.
//! * **Before commit only.** Every reason is decided on a path that returns
//!   before the response head is committed; a committed answer (any status,
//!   any stream) ends the request. Today a node executes no tool, so nothing
//!   with an external side effect can have happened before the head. If that
//!   ever changes, fallback must also stop at that side-effect boundary:
//!   `safe = !response_committed && !external_side_effect_committed`.
//! * **One list, read once, never transitive.** The initial route's list is
//!   copied into the request and followed in order; a fallback route's own
//!   list is never consulted. At most [`MAX_FALLBACK_ROUTES`] entries, so at
//!   most four logical-route attempts.
//! * **Nothing is weakened or re-decided.** Each fallback route runs the same
//!   pipeline with the same request requirements; no classifier, score,
//!   prior, history or latency chooses the next route — the file's order does.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use lightweight_catalog::alias;
use serde::Serialize;
use serde_json::{Value, json};

use crate::auto_route::is_auto;
use crate::config::ConfigError;
use crate::domain::{Route, RouteName};

/// The most fallback routes one list may name: with the initial route, at
/// most four logical-route attempts per request. Fixed, not configurable.
pub const MAX_FALLBACK_ROUTES: usize = 3;

/// The `auto_route.cross_route_fallback` section, as written: an initial
/// route's name, and the routes to try after it, in order.
pub type CrossRouteFallbackFile = BTreeMap<String, Vec<String>>;

/// Why a route attempt may move the request to the next listed route. These
/// three, and only these.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackReason {
    /// Nothing could take the request: no available deployment at planning,
    /// or every attempt failed without an answer (unreachable, transport
    /// error before the head, `404 model_not_found`).
    RouteUnavailable,
    /// Every planned deployment was tried and the route ended holding a
    /// 502/503/504 refusal sent before any answer.
    RouteExhausted,
    /// No available deployment can serve this request's requirements.
    RouteCapabilityMismatch,
}

impl FallbackReason {
    pub const ALL: [Self; 3] = [
        Self::RouteUnavailable,
        Self::RouteExhausted,
        Self::RouteCapabilityMismatch,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RouteUnavailable => "route_unavailable",
            Self::RouteExhausted => "route_exhausted",
            Self::RouteCapabilityMismatch => "route_capability_mismatch",
        }
    }
}

/// The validated section.
#[derive(Clone, Debug, Default)]
pub struct CrossRouteFallback {
    /// Initial route → its ordered fallback routes, spelled as configured.
    chains: Vec<(RouteName, Vec<RouteName>)>,
}

impl CrossRouteFallback {
    /// `route`'s list, or nothing. Read once per request, for the initial
    /// route only.
    pub fn chain(&self, route: &RouteName) -> &[RouteName] {
        self.chains
            .iter()
            .find(|(initial, _)| initial == route)
            .map_or(&[], |(_, chain)| chain.as_slice())
    }

    pub fn chains(&self) -> &[(RouteName, Vec<RouteName>)] {
        &self.chains
    }

    /// The lists, as the admin view shows them.
    pub fn view(&self) -> Value {
        let chains: BTreeMap<&str, Vec<&str>> = self
            .chains
            .iter()
            .map(|(initial, chain)| {
                (
                    initial.as_str(),
                    chain.iter().map(RouteName::as_str).collect(),
                )
            })
            .collect();
        json!(chains)
    }
}

/// One logical-route attempt, in a trace.
#[derive(Clone, Debug, Serialize)]
pub struct FallbackAttemptTrace {
    pub route: String,
    /// `committed` (it answered, whatever the status) or `failed`.
    pub outcome: &'static str,
    /// Why a failed attempt failed: one of the three reasons, or
    /// `context_length_exceeded`, which never moves a request on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
}

/// The `cross_route_fallback` block of a routing trace: present only when a
/// request actually moved to a fallback route. Route names and reasons only.
#[derive(Clone, Debug, Serialize)]
pub struct FallbackTrace {
    pub initial_route: String,
    pub final_route: String,
    /// Every listed route was tried and the last one failed for one of the
    /// three reasons: the client got that route's own error.
    pub exhausted: bool,
    pub attempts: Vec<FallbackAttemptTrace>,
}

/// Check the section against the configured routes.
///
/// `reachable` is every route `Auto` can resolve to; `classifier_routes` the
/// Lightweight classifier's route(s), which are not answering routes.
pub(crate) fn validate(
    raw: &CrossRouteFallbackFile,
    routes: &[Route],
    reachable: &[&RouteName],
    classifier_routes: &[&RouteName],
    errors: &mut Vec<ConfigError>,
) -> Option<CrossRouteFallback> {
    let mut problems: Vec<String> = Vec::new();
    let mut chains: Vec<(RouteName, Vec<RouteName>)> = Vec::new();

    for (source, targets) in raw {
        let initial = match concrete(source, routes, classifier_routes) {
            Ok(route) => route,
            Err(problem) => {
                problems.push(format!("{source:?} {problem}"));
                continue;
            }
        };
        if !reachable.contains(&&initial) {
            problems.push(format!(
                "{source:?} is never a route Auto resolves to, so its list could never be used"
            ));
            continue;
        }
        if chains.iter().any(|(seen, _)| *seen == initial) {
            problems.push(format!(
                "{initial} has more than one list (route names are compared ignoring case)"
            ));
            continue;
        }
        if targets.is_empty() {
            problems.push(format!("{initial}'s list is empty"));
            continue;
        }
        if targets.len() > MAX_FALLBACK_ROUTES {
            problems.push(format!(
                "{initial}'s list names {} routes; at most {MAX_FALLBACK_ROUTES} are allowed",
                targets.len()
            ));
            continue;
        }
        let mut chain: Vec<RouteName> = Vec::with_capacity(targets.len());
        let mut ok = true;
        for target in targets {
            match concrete(target, routes, classifier_routes) {
                Err(problem) => {
                    problems.push(format!("{initial}'s entry {target:?} {problem}"));
                    ok = false;
                }
                Ok(route) if route == initial => {
                    problems.push(format!("{initial} lists itself"));
                    ok = false;
                }
                Ok(route) if chain.contains(&route) => {
                    problems.push(format!(
                        "{initial} lists {route} more than once (route names are compared \
                         ignoring case)"
                    ));
                    ok = false;
                }
                Ok(route) => chain.push(route),
            }
        }
        if ok {
            chains.push((initial, chain));
        }
    }

    if problems.is_empty()
        && let Some(cycle) = find_cycle(&chains)
    {
        problems.push(format!(
            "the fallback lists form a cycle ({cycle}); even though a request only ever \
             follows its initial route's list, a cycle is refused"
        ));
    }

    if !problems.is_empty() {
        errors.extend(
            problems
                .into_iter()
                .map(|problem| ConfigError::BadCrossRouteFallback { problem }),
        );
        return None;
    }
    // In the order the routes are configured, for a stable admin view.
    chains.sort_by_key(|(initial, _)| routes.iter().position(|route| route.name == *initial));
    Some(CrossRouteFallback { chains })
}

/// The configured, concrete, answering route `name` refers to.
fn concrete(
    name: &str,
    routes: &[Route],
    classifier_routes: &[&RouteName],
) -> Result<RouteName, &'static str> {
    if name.trim().is_empty() {
        return Err("is empty; name a route");
    }
    if is_auto(name) {
        return Err("is Auto itself; fallback works between concrete routes");
    }
    if alias::is_reserved(name) {
        return Err("is reserved; name a configured route");
    }
    let route = routes
        .iter()
        .find(|route| route.name.matches(name))
        .map(|route| route.name.clone())
        .ok_or("is not one of the configured routes")?;
    if classifier_routes.contains(&&route) {
        return Err("is the classifier's route, not a route that answers clients");
    }
    Ok(route)
}

/// A cycle in the union of every list (each `initial → entry` is an edge),
/// as `A -> B -> A`, if there is one. Validation only: requests never walk
/// this graph.
fn find_cycle(chains: &[(RouteName, Vec<RouteName>)]) -> Option<String> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        New,
        Open,
        Done,
    }
    fn edges<'a>(chains: &'a [(RouteName, Vec<RouteName>)], node: &RouteName) -> &'a [RouteName] {
        chains
            .iter()
            .find(|(initial, _)| initial == node)
            .map_or(&[], |(_, chain)| chain.as_slice())
    }
    fn visit<'a>(
        chains: &'a [(RouteName, Vec<RouteName>)],
        node: &'a RouteName,
        marks: &mut Vec<(&'a RouteName, Mark)>,
        path: &mut Vec<&'a RouteName>,
    ) -> Option<String> {
        let mark = marks
            .iter()
            .find(|(seen, _)| *seen == node)
            .map_or(Mark::New, |(_, mark)| *mark);
        match mark {
            Mark::Done => return None,
            Mark::Open => {
                let start = path.iter().position(|seen| *seen == node).unwrap_or(0);
                let mut text = String::new();
                for step in &path[start..] {
                    let _ = write!(text, "{step} -> ");
                }
                let _ = write!(text, "{node}");
                return Some(text);
            }
            Mark::New => {}
        }
        marks.push((node, Mark::Open));
        path.push(node);
        for next in edges(chains, node) {
            if let Some(cycle) = visit(chains, next, marks, path) {
                return Some(cycle);
            }
        }
        path.pop();
        if let Some(entry) = marks.iter_mut().find(|(seen, _)| *seen == node) {
            entry.1 = Mark::Done;
        }
        None
    }
    let mut marks = Vec::new();
    for (initial, _) in chains {
        let mut path = Vec::new();
        if let Some(cycle) = visit(chains, initial, &mut marks, &mut path) {
            return Some(cycle);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RouterConfig, RouterFile};
    use serde_json::{Value, json};

    fn file(fallback: Option<Value>) -> RouterFile {
        let mut auto = json!({
            "enabled": true,
            "fallback_route": "General",
            "classifier": {"routes": ["General", "Coder", "Research", "Reasoning"],
                           "lightweight": {"route": "RouterClassifier", "timeout_ms": 5_000}},
            "rules": [
                {"name": "forced-tools", "when": {"tool_choice": "required"}, "route": "ToolAgent"},
                {"name": "semantic", "when": {}, "classify": true}
            ]
        });
        if let Some(fallback) = fallback {
            auto["cross_route_fallback"] = fallback;
        }
        serde_json::from_value(json!({
            "nodes": [{"id": "a", "url": "http://192.0.2.10:11434"}],
            "routes": [
                {"name": "General", "deployments": [{"node": "a", "model": "G"}]},
                {"name": "Coder", "deployments": [{"node": "a", "model": "C"}]},
                {"name": "Research", "deployments": [{"node": "a", "model": "R"}]},
                {"name": "Reasoning", "deployments": [{"node": "a", "model": "S"}]},
                {"name": "ToolAgent", "deployments": [{"node": "a", "model": "T"}]},
                {"name": "Orphan", "deployments": [{"node": "a", "model": "O"}]},
                {"name": "RouterClassifier", "deployments": [{"node": "a", "model": "Q"}]}
            ],
            "auto_route": auto,
        }))
        .expect("the file shape parses")
    }

    fn config(fallback: Value) -> RouterConfig {
        crate::config::validate(file(Some(fallback)), &|_| None).expect("valid")
    }

    fn errors(fallback: Value) -> Vec<String> {
        crate::config::validate(file(Some(fallback)), &|_| None)
            .expect_err("refused")
            .0
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    fn names(chain: &[RouteName]) -> Vec<&str> {
        chain.iter().map(RouteName::as_str).collect()
    }

    fn name(raw: &str) -> RouteName {
        RouteName::parse(raw).unwrap()
    }

    #[test]
    fn without_a_section_there_are_no_lists() {
        let config = crate::config::validate(file(None), &|_| None).unwrap();
        let fallback = config.auto.unwrap().cross_route_fallback;
        assert!(fallback.chains().is_empty());
        assert!(fallback.chain(&name("Coder")).is_empty());
    }

    #[test]
    fn one_and_several_fallbacks_are_accepted_in_order() {
        let fallback = config(json!({"Coder": ["General"]}))
            .auto
            .unwrap()
            .cross_route_fallback;
        assert_eq!(names(fallback.chain(&name("Coder"))), ["General"]);

        let fallback = config(json!({"coder": ["general", "REASONING"], "ToolAgent": ["General"]}))
            .auto
            .unwrap()
            .cross_route_fallback;
        assert_eq!(
            names(fallback.chain(&name("Coder"))),
            ["General", "Reasoning"],
            "spelled as the routes are, in the file's order"
        );
        assert_eq!(names(fallback.chain(&name("ToolAgent"))), ["General"]);
        assert!(fallback.chain(&name("General")).is_empty());
        assert_eq!(
            fallback.view(),
            json!({"Coder": ["General", "Reasoning"], "ToolAgent": ["General"]})
        );
    }

    #[test]
    fn three_entries_is_the_maximum() {
        let _ = config(json!({"Coder": ["General", "Research", "Reasoning"]}));
        let found = errors(json!({"Coder": ["General", "Research", "Reasoning", "ToolAgent"]}));
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].contains("names 4 routes; at most 3 are allowed"),
            "{found:?}"
        );
        assert_eq!(MAX_FALLBACK_ROUTES, 3);
    }

    #[test]
    fn unknown_and_reserved_routes_are_refused() {
        for (fallback, expected) in [
            (
                json!({"Coder": ["Missing"]}),
                "Coder's entry \"Missing\" is not one of the configured routes",
            ),
            (
                json!({"Missing": ["General"]}),
                "\"Missing\" is not one of the configured routes",
            ),
            (
                json!({"Coder": ["Auto"]}),
                "Coder's entry \"Auto\" is Auto itself",
            ),
            (json!({"auto": ["General"]}), "\"auto\" is Auto itself"),
            (
                json!({"Coder": ["default"]}),
                "Coder's entry \"default\" is reserved",
            ),
            (json!({"Coder": [""]}), "Coder's entry \"\" is empty"),
            (
                json!({"Coder": ["RouterClassifier"]}),
                "is the classifier's route",
            ),
            (
                json!({"RouterClassifier": ["General"]}),
                "is the classifier's route",
            ),
            (json!({"Coder": []}), "Coder's list is empty"),
            (
                json!({"Orphan": ["General"]}),
                "\"Orphan\" is never a route Auto resolves to",
            ),
        ] {
            let found = errors(fallback.clone());
            assert!(
                found.iter().any(|e| e.contains(expected)),
                "{fallback}: {expected}: {found:?}"
            );
            assert!(
                found
                    .iter()
                    .all(|e| e.starts_with("auto_route.cross_route_fallback:")),
                "{found:?}"
            );
        }
    }

    #[test]
    fn self_references_and_duplicates_are_refused() {
        let found = errors(json!({"Coder": ["Coder"]}));
        assert!(found[0].contains("Coder lists itself"), "{found:?}");
        let found = errors(json!({"Coder": ["coder"]}));
        assert!(found[0].contains("Coder lists itself"), "{found:?}");
        let found = errors(json!({"Coder": ["General", "General"]}));
        assert!(
            found[0].contains("lists General more than once"),
            "{found:?}"
        );
        let found = errors(json!({"Coder": ["General", "GENERAL"]}));
        assert!(
            found[0].contains("lists General more than once"),
            "case-insensitive: {found:?}"
        );
        let found = errors(json!({"Coder": ["General"], "coder": ["Research"]}));
        assert!(
            found
                .iter()
                .any(|e| e.contains("Coder has more than one list")),
            "{found:?}"
        );
    }

    #[test]
    fn cycles_anywhere_in_the_lists_are_refused() {
        let found = errors(json!({"Coder": ["General"], "General": ["Coder"]}));
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("form a cycle"), "{found:?}");
        assert!(found[0].contains("Coder -> General -> Coder"), "{found:?}");

        let found = errors(json!({
            "Coder": ["Research"], "Research": ["Reasoning"], "Reasoning": ["Coder"]
        }));
        assert!(found[0].contains("form a cycle"), "three routes: {found:?}");

        // A cycle reached only through a later entry of a list.
        let found = errors(json!({
            "Coder": ["General", "Research"], "Research": ["Reasoning"], "Reasoning": ["Research"]
        }));
        assert!(
            found[0].contains("Research -> Reasoning -> Research"),
            "{found:?}"
        );

        // A chain and a shared target are not cycles.
        let _ = config(json!({
            "Coder": ["General", "Reasoning"], "General": ["Research"], "Reasoning": ["Research"]
        }));
    }

    #[test]
    fn unknown_shapes_are_refused() {
        for fallback in [
            json!({"Coder": "General"}),
            json!({"Coder": {"on_unavailable": ["General"]}}),
            json!(["Coder", "General"]),
            json!({"Coder": [1]}),
        ] {
            let parsed: Result<RouterFile, _> = serde_json::from_value(json!({
                "nodes": [], "routes": [],
                "auto_route": {"fallback_route": "General", "cross_route_fallback": fallback}
            }));
            assert!(parsed.is_err(), "{fallback}");
        }
    }

    #[test]
    fn the_reasons_are_exactly_three() {
        let names: Vec<&str> = FallbackReason::ALL.iter().map(|r| r.as_str()).collect();
        assert_eq!(
            names,
            [
                "route_unavailable",
                "route_exhausted",
                "route_capability_mismatch"
            ]
        );
    }
}
