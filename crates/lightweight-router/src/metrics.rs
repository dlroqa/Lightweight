//! The router's own numbers, in Prometheus text.
//!
//! The gateway already serves `/metrics`, so the router does too, in the same
//! format, under names of its own. Labels are only ever configured names — a
//! route, a node — never anything a client typed: a request for a route that
//! does not exist is counted under `_unknown`, so a client cannot grow the
//! label set by inventing model names.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

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
    active: Arc<AtomicU64>,
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
        let guard = metrics.enter();
        let text = metrics.to_prometheus(&BTreeMap::new(), &BTreeMap::new());
        assert!(text.contains("router_requests_total{route=\"Coder\",outcome=\"ok\"} 2"));
        assert!(text.contains("router_failovers_total{route=\"Coder\"} 1"));
        assert!(text.contains("router_active_requests 1"));
        assert!(text.contains(
            "router_capability_filtered_total{route=\"Coder\",reason=\"tools_unsupported\"} 1"
        ));
        assert!(text.contains("router_capability_mismatch_total{route=\"Coder\"} 1"));
        drop(guard);
        assert_eq!(metrics.active(), 0);
    }

    #[test]
    fn a_quote_in_a_route_name_cannot_break_the_format() {
        assert_eq!(escape("say \"hi\""), "say \\\"hi\\\"");
    }
}
