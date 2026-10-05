//! What happened to one request, start to finish.
//!
//! [`crate::domain::RoutingDecision`] says where one attempt went and why. A
//! [`RoutingTrace`] says what happened over the whole request: what was
//! available, what this request ruled out, whether its session's affinity held,
//! every attempt and how it ended, where it was finally answered, how long the
//! first token and the whole response took, and how close the router's prompt
//! estimate was to the node's own count. One per routed request, written to the
//! log when the request ends and kept in a small ring for
//! `GET /api/router/v1/traces`.
//!
//! Never in a trace: a prompt, a message, a tool argument, a credential, a
//! session id (only the keyed fingerprint [`crate::affinity::AffinityKey`]
//! gives), or a client address. Memory only, bounded by the configured
//! capacity, oldest dropped first.

use std::collections::VecDeque;
use std::sync::{Mutex, PoisonError};

use serde::Serialize;

/// One request's session, as far as affinity went.
#[derive(Clone, Debug, Serialize)]
pub struct SessionTrace {
    /// The keyed fingerprint, never the id.
    pub fingerprint: String,
    /// `hit`, `miss`, `reassigned`, or `none` when affinity could not apply
    /// (nothing committed).
    pub affinity: &'static str,
    /// The deployment the session was sticky to when the request arrived.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sticky: Option<String>,
    /// Why the session moved, when it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reassignment: Option<&'static str>,
}

/// One deployment the request was sent to.
#[derive(Clone, Debug, Serialize)]
pub struct AttemptTrace {
    pub deployment: String,
    /// The routing reason for this attempt.
    pub reason: &'static str,
    /// `committed`, `failed` (no answer, or one that moved the request on), or
    /// `context_overflow`.
    pub outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_status: Option<u16>,
    /// From sending to the response head, when there was one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_ms: Option<f64>,
}

/// A deployment the request ruled out, and every reason.
#[derive(Clone, Debug, Serialize)]
pub struct ExcludedTrace {
    pub deployment: String,
    pub reasons: Vec<&'static str>,
}

/// A context overflow fallback: the router's estimate, every context that
/// proved too small, and the one that answered.
#[derive(Clone, Debug, Default, Serialize)]
pub struct OverflowTrace {
    pub estimated_prompt_tokens: Option<u32>,
    pub too_small: Vec<OverflowStep>,
    pub answered_context: Option<u32>,
}

#[derive(Clone, Debug, Serialize)]
pub struct OverflowStep {
    pub deployment: String,
    pub context: Option<u32>,
}

/// One routed request, start to finish.
#[derive(Clone, Debug, Serialize)]
pub struct RoutingTrace {
    pub request_id: String,
    /// Unix seconds when the router had the request.
    pub received_at: u64,
    pub route: String,
    pub endpoint: &'static str,
    pub stream: bool,
    pub policy: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionTrace>,
    /// Deployments the route has.
    pub deployments: usize,
    /// Of those, the ones available by health.
    pub available: usize,
    /// Of those, the ones able to serve this request.
    pub capable: usize,
    pub unavailable: Vec<ExcludedTrace>,
    pub unfit: Vec<ExcludedTrace>,
    /// The first choice, and why it was made.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection_reason: Option<&'static str>,
    pub attempts: Vec<AttemptTrace>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_deployment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_overflow: Option<OverflowTrace>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_prompt_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_prompt_tokens: Option<u32>,
    /// Parse, requirements and plan: the router's own time.
    pub routing_ms: f64,
    /// Streaming only, and only once output arrived.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<f64>,
    pub duration_ms: f64,
    /// The status the client got, when it got one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// `ok`, `client_error`, `server_error`, `unavailable`, `interrupted` (a
    /// committed stream the node broke off), or `cancelled` (the client left).
    pub outcome: &'static str,
}

impl RoutingTrace {
    pub fn new(
        request_id: &str,
        route: &str,
        endpoint: &'static str,
        policy: &'static str,
    ) -> Self {
        Self {
            request_id: request_id.to_owned(),
            received_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs())
                .unwrap_or_default(),
            route: route.to_owned(),
            endpoint,
            stream: false,
            policy,
            session: None,
            deployments: 0,
            available: 0,
            capable: 0,
            unavailable: Vec::new(),
            unfit: Vec::new(),
            selected: None,
            selection_reason: None,
            attempts: Vec::new(),
            final_deployment: None,
            context_overflow: None,
            estimated_prompt_tokens: None,
            actual_prompt_tokens: None,
            routing_ms: 0.0,
            ttft_ms: None,
            duration_ms: 0.0,
            status: None,
            outcome: "cancelled",
        }
    }
}

/// The most recent traces, oldest dropped first.
#[derive(Debug)]
pub struct TraceBook {
    capacity: usize,
    traces: Mutex<VecDeque<RoutingTrace>>,
}

impl TraceBook {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            traces: Mutex::new(VecDeque::with_capacity(capacity.min(1_024))),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn push(&self, trace: RoutingTrace) {
        if self.capacity == 0 {
            return;
        }
        let mut traces = self.traces.lock().unwrap_or_else(PoisonError::into_inner);
        while traces.len() >= self.capacity {
            traces.pop_front();
        }
        traces.push_back(trace);
    }

    /// Up to `limit` traces, newest first.
    pub fn recent(&self, limit: usize) -> Vec<RoutingTrace> {
        self.traces
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .rev()
            .take(limit)
            .cloned()
            .collect()
    }

    pub fn len(&self) -> usize {
        self.traces
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trace(id: &str) -> RoutingTrace {
        RoutingTrace::new(id, "Coder", "chat", "priority")
    }

    #[test]
    fn the_ring_keeps_only_the_newest_and_returns_them_newest_first() {
        let book = TraceBook::new(3);
        for id in ["a", "b", "c", "d", "e"] {
            book.push(trace(id));
        }
        assert_eq!(book.len(), 3);
        let ids: Vec<String> = book.recent(10).into_iter().map(|t| t.request_id).collect();
        assert_eq!(ids, ["e", "d", "c"]);
        assert_eq!(book.recent(1).len(), 1);
    }

    #[test]
    fn a_zero_capacity_keeps_nothing() {
        let book = TraceBook::new(0);
        book.push(trace("a"));
        assert!(book.is_empty());
    }

    #[test]
    fn an_unset_trace_serializes_without_empty_optionals() {
        let text = serde_json::to_string(&trace("a")).unwrap();
        assert!(!text.contains("session"));
        assert!(!text.contains("ttft_ms"));
        assert!(text.contains("\"outcome\":\"cancelled\""));
    }
}
