//! The invoker the loop drives.
//!
//! [`BoundedExecutor`] is the concrete [`ToolInvoker`] that closes the
//! model→tool→model loop: it declares the registry's schemas, classifies each
//! call through a [`PolicyEngine`] so a risky one pauses the run rather than
//! running unasked, and runs an approved call under a per-call timeout, prompt
//! cancellation and an output ceiling. Arguments are parsed and schema-validated
//! before a tool ever sees them, so a malformed call becomes a result the model
//! is shown, never a panic.

use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use lightagent_core::{
    ApprovalDecision, ApprovalNeed, ApprovalRecord, ApprovalRequest, PolicyEngine, RiskClass,
    RunId, Scope, ToolCall, ToolInvoker, ToolOutcome, ToolSchema,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::context::{Clock, Delegation, ToolCtx, WebContext};
use crate::output::clamp;
use crate::registry::ToolRegistry;
use crate::schema;

/// A safe argument preview is capped at this many bytes before it reaches a
/// prompt, a log or an approval UI.
const PREVIEW_MAX_BYTES: usize = 200;

/// Runs the tools in a [`ToolRegistry`] under a policy and hard bounds.
pub struct BoundedExecutor {
    registry: ToolRegistry,
    policy: Mutex<PolicyEngine>,
    per_call: Duration,
    max_output_bytes: usize,
    run: Option<RunId>,
    clock: Clock,
    delegation: Option<Delegation>,
    web: Option<WebContext>,
}

impl BoundedExecutor {
    /// An executor over `registry`, enforcing `policy` and the given bounds.
    pub fn new(
        registry: ToolRegistry,
        policy: PolicyEngine,
        per_call: Duration,
        max_output_bytes: usize,
    ) -> Self {
        Self {
            registry,
            policy: Mutex::new(policy),
            per_call,
            max_output_bytes,
            run: None,
            clock: Clock::System,
            delegation: None,
            web: None,
        }
    }

    /// Name the run these tools belong to (a child run's `parent`).
    pub fn with_run(mut self, run: RunId) -> Self {
        self.run = Some(run);
        self
    }

    /// Set the clock time-reading tools observe.
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// Enable `agent.delegate` by supplying what a worker run needs.
    pub fn with_delegation(mut self, delegation: Delegation) -> Self {
        self.delegation = Some(delegation);
        self
    }

    /// Enable `web.fetch`/`web.search` by supplying the HTTP client and policy.
    pub fn with_web(mut self, web: WebContext) -> Self {
        self.web = Some(web);
        self
    }

    /// The risk and scopes a call would carry, from its tool's definition.
    fn classify(&self, call: &ToolCall) -> Option<(RiskClass, Vec<Scope>)> {
        self.registry
            .get(&call.name)
            .map(|tool| (tool.definition().risk, tool.definition().scopes.clone()))
    }

    /// Build the approval request for a known call.
    fn request_for(&self, call: &ToolCall, risk: RiskClass, scopes: Vec<Scope>) -> ApprovalRequest {
        ApprovalRequest::new(call.name.clone(), risk, scopes, preview(&call.arguments))
    }

    fn ctx(&self, cancel: CancellationToken) -> ToolCtx {
        let mut ctx = ToolCtx::new(cancel).with_clock(self.clock.clone());
        if let Some(run) = &self.run {
            ctx = ctx.with_run(run.clone());
        }
        if let Some(delegation) = &self.delegation {
            ctx = ctx.with_delegation(delegation.clone());
        }
        if let Some(web) = &self.web {
            ctx = ctx.with_web(web.clone());
        }
        ctx
    }
}

#[async_trait]
impl ToolInvoker for BoundedExecutor {
    fn schemas(&self) -> Vec<ToolSchema> {
        self.registry.schemas()
    }

    fn approval_for(&self, call: &ToolCall) -> ApprovalNeed {
        // An undeclared tool is not blocked here; it is rejected at `invoke`
        // with a controlled result, so approval only ever gates a real tool.
        let Some((risk, scopes)) = self.classify(call) else {
            return ApprovalNeed::AutoApprove;
        };
        let request = self.request_for(call, risk, scopes);
        match self.policy.lock() {
            Ok(policy) => policy.evaluate(&request, self.clock.now()),
            Err(_) => ApprovalNeed::Deny("the approval policy is unavailable".into()),
        }
    }

    async fn invoke(&self, call: &ToolCall, cancel: CancellationToken) -> ToolOutcome {
        let Some(tool) = self.registry.get(&call.name) else {
            return ToolOutcome::error(format!("the tool '{}' is not available", call.name));
        };

        let args = match parse_arguments(&call.arguments) {
            Ok(args) => args,
            Err(message) => return ToolOutcome::error(message),
        };

        if let Err(errors) = schema::validate(&tool.definition().parameters, &args) {
            let detail = errors
                .iter()
                .map(|error| error.to_string())
                .collect::<Vec<_>>()
                .join("; ");
            return ToolOutcome::error(format!(
                "arguments for '{}' are invalid: {detail}",
                call.name
            ));
        }

        let ctx = self.ctx(cancel.clone());
        let outcome = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                ToolOutcome::error(format!("the tool '{}' was cancelled", call.name))
            }
            result = tokio::time::timeout(self.per_call, tool.call(&args, &ctx)) => match result {
                Ok(outcome) => outcome,
                Err(_) => ToolOutcome::error(format!(
                    "the tool '{}' timed out after {:?}",
                    call.name, self.per_call
                )),
            }
        };

        ToolOutcome {
            content: clamp(outcome.content, self.max_output_bytes),
            is_error: outcome.is_error,
        }
    }

    fn remember(&self, decision: &ApprovalDecision, call: &ToolCall) {
        if !decision.granted {
            return;
        }
        let Some((risk, scopes)) = self.classify(call) else {
            return;
        };
        let request = self.request_for(call, risk, scopes);
        let record = ApprovalRecord::from_request(&request, self.clock.now(), decision.remember);
        if let Ok(mut policy) = self.policy.lock() {
            policy.remember(record);
        }
    }
}

/// Parse a tool call's raw argument string into a JSON value.
///
/// An empty or whitespace-only string is the no-argument case and reads as `{}`,
/// which a `{ "type": "object" }` schema with no required fields accepts.
fn parse_arguments(raw: &str) -> Result<Value, String> {
    if raw.trim().is_empty() {
        return Ok(Value::Object(serde_json::Map::new()));
    }
    serde_json::from_str(raw).map_err(|error| format!("arguments were not valid JSON: {error}"))
}

/// A bounded, secret-redacted rendering of a call's arguments, safe to show.
fn preview(arguments: &str) -> String {
    let rendered = match serde_json::from_str::<Value>(arguments) {
        Ok(value) => redact(value).to_string(),
        Err(_) => arguments.to_string(),
    };
    clamp(rendered, PREVIEW_MAX_BYTES)
}

/// Replace secret-looking values with a placeholder, recursively.
fn redact(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, child)| {
                    if is_secret_key(&key) {
                        (key, Value::String("<redacted>".into()))
                    } else {
                        (key, redact(child))
                    }
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(redact).collect()),
        other => other,
    }
}

fn is_secret_key(key: &str) -> bool {
    let lowered = key.to_ascii_lowercase();
    [
        "key",
        "secret",
        "token",
        "password",
        "passwd",
        "credential",
        "authorization",
    ]
    .iter()
    .any(|needle| lowered.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_arguments_read_as_an_empty_object() {
        assert_eq!(
            parse_arguments("").unwrap(),
            Value::Object(Default::default())
        );
        assert_eq!(
            parse_arguments("  ").unwrap(),
            Value::Object(Default::default())
        );
    }

    #[test]
    fn malformed_arguments_are_reported_not_raised() {
        let error = parse_arguments("{not json").unwrap_err();
        assert!(error.contains("not valid JSON"));
    }

    #[test]
    fn preview_redacts_secret_looking_keys() {
        let out = preview(r#"{"api_key":"sk-123","city":"Paris"}"#);
        assert!(out.contains("<redacted>"));
        assert!(!out.contains("sk-123"));
        assert!(out.contains("Paris"));
    }
}
