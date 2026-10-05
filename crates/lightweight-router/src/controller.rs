//! The placement controller: a loop beside the router, never inside a request.
//!
//! ```text
//! observe (the health book the router already keeps)
//!    → compare each route's ready deployments with its target
//!    → plan loads (crate::placement::plan: deterministic, bounded)
//!    → start each as its own task
//!    → wait for the next interval, or an operator's reconcile
//! ```
//!
//! One load, against the node's own control API (`/api/v1`, with the node's
//! own credential — never the router's client key):
//!
//! 1. `GET /api/v1/models`: the model must be in the node's catalog with its
//!    file present. Nothing is downloaded. A model the node already reports
//!    `loaded` is not loaded again; only its readiness is confirmed.
//! 2. `POST /api/v1/models/{id}/load` with an empty body: the node chooses
//!    context, slots and threads, and its admission control decides whether
//!    the model fits. The router estimates nothing.
//! 3. `GET /api/v1/jobs/{job}` until the job ends. A failure carries the
//!    node's structured `error.code`, kept as the reason.
//! 4. The node is probed exactly as the health monitor probes it, until the
//!    deployment is available by the same rule the request path uses. Only
//!    then is the load counted a success.
//!
//! The whole of a load is bounded by `placement.load_timeout_secs`. A failure
//! backs the deployment off (see [`crate::placement::PlacementBook`]). Every
//! load is a task the loop owns: stopping the router aborts them, and the
//! router has no child process to leave behind — a load a node already
//! accepted simply finishes on the node.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use lightweight_catalog::alias;
use lightweight_observability::targets;
use serde_json::Value;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::RouterState;
use crate::domain::{DeploymentHealth, Node};
use crate::health::{availability, probe};
use crate::placement::{Action, FailureReason, LastAction, plan};

/// How often a load job and, after it, the node's readiness are re-read.
const POLL: Duration = Duration::from_millis(500);
/// How long one control-API call may take. A load itself is a job, so no
/// single call should take long.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);

/// Start the controller, if any route asks for placement.
pub fn spawn(
    state: Arc<RouterState>,
    stop: CancellationToken,
) -> Option<tokio::task::JoinHandle<()>> {
    crate::placement::configured(&state.topology).then(|| tokio::spawn(run(state, stop)))
}

async fn run(state: Arc<RouterState>, stop: CancellationToken) {
    let policy = *state.placement.policy();
    tracing::info!(
        target: targets::ROUTER,
        interval_secs = policy.interval.as_secs(),
        load_timeout_secs = policy.load_timeout.as_secs(),
        "placement controller started"
    );
    let mut tick = tokio::time::interval(policy.interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut loads: JoinSet<()> = JoinSet::new();
    loop {
        tokio::select! {
            () = stop.cancelled() => break,
            _ = tick.tick() => {}
            () = state.placement.wake.notified() => {}
            // A finished load frees its node: plan again at once rather than
            // waiting out the interval.
            Some(_) = loads.join_next(), if !loads.is_empty() => {}
        }
        reconcile(&state, &mut loads);
    }
    // Abandon what is in flight. A node that already accepted a load finishes
    // it on its own; the next router to start observes the result.
    loads.abort_all();
    while loads.join_next().await.is_some() {}
    for id in state.placement.loading() {
        state.placement.abandon(&id);
    }
    tracing::info!(target: targets::ROUTER, "placement controller stopped");
}

/// One pass: plan from what the router has observed, and start the loads.
/// Returns at once; the loads run as tasks.
fn reconcile(state: &Arc<RouterState>, loads: &mut JoinSet<()>) {
    let started = Instant::now();
    let health = state.health.snapshot();
    let loading = state.placement.loading();
    let actions = plan(&state.topology, &health, &loading, &|id| {
        state.placement.backing_off(id, started)
    });
    for action in actions {
        if !state.placement.begin(&action.deployment, started) {
            continue;
        }
        tracing::info!(
            target: targets::ROUTER,
            route = %action.route,
            node = %action.node,
            deployment = %action.deployment,
            model = action.model.as_str(),
            action = "load",
            "placement: loading a deployment toward its route's target"
        );
        loads.spawn(load(Arc::clone(state), action));
    }
    state.metrics.observe_reconcile(started.elapsed());
    state.placement.passed(SystemTime::now());
}

/// How a load attempt went.
type Attempt = Result<&'static str, (FailureReason, Option<String>)>;

async fn load(state: Arc<RouterState>, action: Action) {
    let started = Instant::now();
    let deadline = started + state.placement.policy().load_timeout;
    let outcome =
        match tokio::time::timeout_at(deadline.into(), attempt(&state, &action, deadline)).await {
            Ok(outcome) => outcome,
            Err(_) => Err((FailureReason::LoadTimeout, None)),
        };
    let elapsed = started.elapsed();
    let at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or_default();
    let duration_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
    let route = action.route.as_str();
    match outcome {
        Ok(result) => {
            tracing::info!(
                target: targets::ROUTER,
                route,
                node = %action.node,
                deployment = %action.deployment,
                model = action.model.as_str(),
                action = "load",
                result,
                duration_ms,
                "placement: deployment is ready"
            );
            state.metrics.record_placement_action(route, "load", result);
            state.placement.succeeded(
                &action.deployment,
                LastAction {
                    action: "load",
                    result,
                    reason: None,
                    code: None,
                    at,
                    duration_ms,
                },
            );
        }
        Err((reason, code)) => {
            let retry_in = state.placement.failed(
                &action.deployment,
                LastAction {
                    action: "load",
                    result: "failed",
                    reason: Some(reason),
                    code: code.clone(),
                    at,
                    duration_ms,
                },
                Instant::now(),
            );
            tracing::warn!(
                target: targets::ROUTER,
                route,
                node = %action.node,
                deployment = %action.deployment,
                model = action.model.as_str(),
                action = "load",
                result = "failed",
                reason = reason.as_str(),
                code = code.as_deref(),
                duration_ms,
                retry_in_secs = retry_in.as_secs(),
                "placement: load failed; backing off"
            );
            state
                .metrics
                .record_placement_action(route, "load", "failed");
            state
                .metrics
                .record_placement_failure(route, reason.as_str());
        }
    }
}

async fn attempt(state: &RouterState, action: &Action, deadline: Instant) -> Attempt {
    let node = state
        .topology
        .node(&action.node)
        .ok_or((FailureReason::LoadRejected, None))?;

    // 1. Is it installed, and is it already loaded?
    let rows = control_get(state, node, "/api/v1/models").await?;
    let row = rows["data"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|row| {
            row["id"]
                .as_str()
                .is_some_and(|id| alias::same_name(id, &action.model))
                || row["alias"]
                    .as_str()
                    .is_some_and(|name| alias::same_name(name, &action.model))
        })
        .ok_or((FailureReason::ModelNotInstalled, None))?;
    let catalog_id = row["id"]
        .as_str()
        .ok_or((FailureReason::ModelNotInstalled, None))?
        .to_owned();
    let result = match row["state"].as_str() {
        Some("missing") => {
            return Err((
                FailureReason::ModelNotInstalled,
                Some("model_file_not_found".into()),
            ));
        }
        // Loaded already (by an operator, or a load the last pass did not see
        // finish): nothing to load, only readiness to confirm.
        Some("loaded") => "already_loaded",
        _ => {
            // 2. Ask the node to load it; 3. wait for the job.
            let job = start_load(state, node, &catalog_id).await?;
            wait_for_job(state, node, job).await?;
            "succeeded"
        }
    };

    // 4. Ready only when the router itself sees it, by the request path's rule.
    let deployment = state
        .topology
        .deployment(&action.deployment)
        .ok_or((FailureReason::LoadRejected, None))?;
    loop {
        let outcome = probe(&state.client, node, state.policy.timeout).await;
        state.health.record(&node.id, outcome);
        if availability(node, deployment, &state.health.status(&node.id))
            == DeploymentHealth::Available
        {
            return Ok(result);
        }
        if Instant::now() + POLL >= deadline {
            return Err((FailureReason::LoadTimeout, Some("not_ready".into())));
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn start_load(
    state: &RouterState,
    node: &Node,
    catalog_id: &str,
) -> Result<u64, (FailureReason, Option<String>)> {
    let path = format!("/api/v1/models/{}/load", encode_segment(catalog_id));
    let mut request = state
        .client
        .post(node.endpoint(&path))
        .timeout(CONTROL_TIMEOUT);
    if let Some(value) = node.auth.header_value() {
        request = request.header(reqwest::header::AUTHORIZATION, value);
    }
    let response = request
        .send()
        .await
        .map_err(|_| (FailureReason::NodeUnhealthy, None))?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    if status.as_u16() != 202 {
        let code = error_code(&body);
        let reason =
            code.as_deref().map_or(
                FailureReason::LoadRejected,
                |code| match FailureReason::of_code(code) {
                    // A code the mapping does not know, on a refused request, is a
                    // refusal rather than a model failure.
                    FailureReason::ModelFailed => FailureReason::LoadRejected,
                    reason => reason,
                },
            );
        return Err((
            reason,
            code.or_else(|| Some(format!("http_{}", status.as_u16()))),
        ));
    }
    body["job"]
        .as_u64()
        .ok_or((FailureReason::LoadRejected, Some("no_job".into())))
}

async fn wait_for_job(
    state: &RouterState,
    node: &Node,
    job: u64,
) -> Result<(), (FailureReason, Option<String>)> {
    let path = format!("/api/v1/jobs/{job}");
    loop {
        let body = control_get(state, node, &path).await?;
        match body["status"]["state"].as_str() {
            Some("succeeded") => return Ok(()),
            Some("failed") => {
                let code = body["status"]["error"]["code"].as_str().map(str::to_owned);
                let reason = code
                    .as_deref()
                    .map_or(FailureReason::ModelFailed, FailureReason::of_code);
                return Err((reason, code));
            }
            Some("cancelled") => {
                return Err((FailureReason::ModelFailed, Some("cancelled".into())));
            }
            _ => tokio::time::sleep(POLL).await,
        }
    }
}

async fn control_get(
    state: &RouterState,
    node: &Node,
    path: &str,
) -> Result<Value, (FailureReason, Option<String>)> {
    let mut request = state
        .client
        .get(node.endpoint(path))
        .timeout(CONTROL_TIMEOUT);
    if let Some(value) = node.auth.header_value() {
        request = request.header(reqwest::header::AUTHORIZATION, value);
    }
    let response = request
        .send()
        .await
        .map_err(|_| (FailureReason::NodeUnhealthy, None))?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let code = error_code(&body).or_else(|| Some(format!("http_{}", status.as_u16())));
        return Err((FailureReason::LoadRejected, code));
    }
    Ok(body)
}

fn error_code(body: &Value) -> Option<String> {
    body["error"]["code"].as_str().map(str::to_owned)
}

/// Percent-encode one path segment: a catalog id is a slug, but nothing is
/// put in a URL path unescaped on that assumption.
fn encode_segment(segment: &str) -> String {
    segment
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_segment_is_escaped() {
        assert_eq!(encode_segment("smollm2-135m_q4.k"), "smollm2-135m_q4.k");
        assert_eq!(encode_segment("a/b c@8k"), "a%2Fb%20c%408k");
    }
}
