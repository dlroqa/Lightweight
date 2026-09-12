//! The invoker the loop drives.
//!
//! [`BoundedExecutor`] is the concrete [`ToolInvoker`] that closes the
//! model→tool→model loop: it declares the registry's schemas, classifies each
//! call through a [`PolicyEngine`] so a risky one pauses the run rather than
//! running unasked, and runs an approved call under a per-call timeout, prompt
//! cancellation and an output ceiling. Arguments are parsed and schema-validated
//! before a tool ever sees them, so a malformed call becomes a result the model
//! is shown, never a panic.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use lightagent_core::{
    ApprovalDecision, ApprovalNeed, ApprovalRecord, ApprovalRequest, PolicyEngine, RiskClass,
    RunId, Scope, ToolCall, ToolInvoker, ToolOutcome, ToolSchema,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::context::{Clock, Delegation, SkillContext, ToolCtx, WebContext, WorkspaceContext};
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
    pending: Mutex<HashMap<String, (ApprovalRequest, String)>>,
    approved: Mutex<HashMap<String, String>>,
    per_call: Duration,
    max_output_bytes: usize,
    run: Option<RunId>,
    clock: Clock,
    delegation: Option<Delegation>,
    web: Option<WebContext>,
    workspace: Option<WorkspaceContext>,
    skills: Option<SkillContext>,
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
            pending: Mutex::new(HashMap::new()),
            approved: Mutex::new(HashMap::new()),
            per_call,
            max_output_bytes,
            run: None,
            clock: Clock::System,
            delegation: None,
            web: None,
            workspace: None,
            skills: None,
        }
    }

    /// Name the run these tools belong to (a child run's `parent`).
    pub fn with_run(mut self, run: RunId) -> Self {
        self.run = Some(run);
        self
    }

    /// Reset the in-memory approval policy when an interactive caller starts a
    /// genuinely new session. No unrelated workspace or tool state is changed.
    pub fn reset_session_policy(&self, policy: PolicyEngine) {
        if let Ok(mut current) = self.policy.lock() {
            *current = policy;
        }
        if let Ok(mut pending) = self.pending.lock() {
            pending.clear();
        }
        if let Ok(mut approved) = self.approved.lock() {
            approved.clear();
        }
    }

    /// Restore a session's explicit unrestricted approval on resume.
    pub fn allow_without_restrictions(&self) {
        if let Ok(mut policy) = self.policy.lock() {
            policy.allow_without_restrictions();
        }
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

    /// Enable the `fs.*`/`terminal.run` tools by supplying the confined workspace.
    pub fn with_workspace(mut self, workspace: WorkspaceContext) -> Self {
        self.workspace = Some(workspace);
        self
    }

    /// Make skills available to `skill.read`.
    pub fn with_skills(mut self, skills: SkillContext) -> Self {
        self.skills = Some(skills);
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
        let arguments_preview = match call.name.as_str() {
            "fs.write" => serde_json::from_str::<Value>(&call.arguments)
                .ok()
                .map(|args| {
                    let path = args
                        .get("path")
                        .and_then(Value::as_str)
                        .unwrap_or("<missing>");
                    let bytes = args
                        .get("content")
                        .and_then(Value::as_str)
                        .map(str::len)
                        .unwrap_or(0);
                    let mode = if args.get("append").and_then(Value::as_bool) == Some(true) {
                        "append"
                    } else {
                        "write"
                    };
                    format!("path={path:?}, mode={mode}, content_bytes={bytes}")
                })
                .unwrap_or_else(|| preview(&call.arguments)),
            "terminal.run" => serde_json::from_str::<Value>(&call.arguments)
                .ok()
                .map(|args| redact(args).to_string())
                .unwrap_or_else(|| preview(&call.arguments)),
            _ => preview(&call.arguments),
        };
        ApprovalRequest::new(call.name.clone(), risk, scopes, arguments_preview)
    }

    fn blocked_terminal_call(&self, call: &ToolCall) -> Option<String> {
        if call.name != "terminal.run" {
            return None;
        }
        let args = serde_json::from_str::<Value>(&call.arguments).ok()?;
        let command = args.get("command")?.as_str()?;
        if redact(args.clone()).to_string().len() > 512 {
            return Some("terminal arguments exceed the review limit".to_owned());
        }
        crate::builtins::terminal::blocked_command(command).map(str::to_owned)
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
        if let Some(workspace) = &self.workspace {
            ctx = ctx.with_workspace(workspace.clone());
        }
        if let Some(skills) = &self.skills {
            ctx = ctx.with_skills(skills.clone());
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
        if let Some(reason) = self.blocked_terminal_call(call) {
            return ApprovalNeed::Deny(reason);
        }
        let request = self.request_for(call, risk, scopes);
        let need = match self.policy.lock() {
            Ok(policy) => policy.evaluate(&request, self.clock.now()),
            Err(_) => ApprovalNeed::Deny("the approval policy is unavailable".into()),
        };
        if let ApprovalNeed::Require(request) = &need {
            if let Ok(mut pending) = self.pending.lock() {
                pending.insert(
                    call.id.clone(),
                    (
                        request.clone(),
                        format!("{}\0{}", call.name, call.arguments),
                    ),
                );
            } else {
                return ApprovalNeed::Deny("the approval tracker is unavailable".into());
            }
        }
        need
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

        if let Some(reason) = self.blocked_terminal_call(call) {
            return ToolOutcome::error(reason);
        }
        let request = self.request_for(
            call,
            tool.definition().risk,
            tool.definition().scopes.clone(),
        );
        let need = match self.policy.lock() {
            Ok(policy) => policy.evaluate(&request, self.clock.now()),
            Err(_) => return ToolOutcome::error("the approval policy is unavailable"),
        };
        match need {
            ApprovalNeed::Deny(reason) => return ToolOutcome::error(reason),
            ApprovalNeed::Require(_) => {
                let key = format!("{}\0{}", call.name, call.arguments);
                let allowed = self
                    .approved
                    .lock()
                    .ok()
                    .and_then(|mut approved| approved.remove(&call.id))
                    .is_some_and(|approved| approved == key);
                if !allowed {
                    return ToolOutcome::error(format!(
                        "the tool '{}' requires approval for this call",
                        call.name
                    ));
                }
            }
            ApprovalNeed::AutoApprove => {
                if let Ok(mut approved) = self.approved.lock() {
                    approved.remove(&call.id);
                }
            }
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
        let pending = self
            .pending
            .lock()
            .ok()
            .and_then(|mut pending| pending.remove(&call.id));
        let key = format!("{}\0{}", call.name, call.arguments);
        let Some((request, _)) = pending.filter(|(request, pending_key)| {
            request.id == decision.id && request.tool == call.name && pending_key == &key
        }) else {
            return;
        };
        if let Ok(mut approved) = self.approved.lock() {
            approved.insert(call.id.clone(), key);
        } else {
            return;
        }
        if decision.unrestricted {
            if let Ok(mut policy) = self.policy.lock() {
                policy.allow_without_restrictions();
            }
            return;
        }
        let Some(ttl) = decision.remember else {
            return;
        };
        if matches!(
            request.risk,
            RiskClass::Mutating | RiskClass::Executable | RiskClass::Privileged
        ) || request
            .scopes
            .iter()
            .any(|scope| matches!(scope.as_str(), "fs:write" | "terminal:exec"))
        {
            return;
        }
        let record = ApprovalRecord::from_request(&request, self.clock.now(), Some(ttl));
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

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: "call-1".to_owned(),
            name: name.to_owned(),
            arguments: "{}".to_owned(),
        }
    }

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

    #[test]
    fn one_time_and_unrestricted_approval_modes_are_distinct() {
        let executor = BoundedExecutor::new(
            ToolRegistry::builtin(),
            PolicyEngine::new(lightagent_core::ApprovalPolicy::Strict.into()),
            Duration::from_secs(1),
            1024,
        );
        let write = call("fs.write");
        let ApprovalNeed::Require(request) = executor.approval_for(&write) else {
            panic!("strict policy should request approval");
        };

        executor.remember(&ApprovalDecision::grant(request.id), &write);
        assert!(matches!(
            executor.approval_for(&write),
            ApprovalNeed::Require(_)
        ));

        let ApprovalNeed::Require(request) = executor.approval_for(&write) else {
            panic!("one-time approval must not change policy");
        };
        executor.remember(&ApprovalDecision::grant_unrestricted(request.id), &write);
        assert!(matches!(
            executor.approval_for(&call("terminal.run")),
            ApprovalNeed::Require(_)
        ));
        assert_eq!(
            executor.approval_for(&call("datetime.now")),
            ApprovalNeed::AutoApprove
        );
    }

    #[tokio::test]
    async fn execution_boundary_requires_a_matching_one_time_approval() {
        let root = std::env::temp_dir().join(format!(
            "lightagent-approval-{}",
            lightagent_core::RunId::new().as_str()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let executor = BoundedExecutor::new(
            ToolRegistry::builtin(),
            PolicyEngine::new(lightagent_core::permissions::ApprovalPolicy::permissive()),
            Duration::from_secs(1),
            1024,
        )
        .with_workspace(WorkspaceContext {
            workspace: std::sync::Arc::new(crate::Workspace::new(&root).unwrap()),
            policy: std::sync::Arc::new(crate::WorkspacePolicy {
                max_file_bytes: 1024,
                allow_terminal: true,
                terminal_timeout: Duration::from_secs(1),
                terminal_allowlist: Vec::new(),
            }),
        });
        let write = ToolCall {
            id: "write-1".to_owned(),
            name: "fs.write".to_owned(),
            arguments: r#"{"path":"note.txt","content":"hello"}"#.to_owned(),
        };
        let cancel = CancellationToken::new();
        assert!(
            executor
                .invoke(&write, cancel.clone())
                .await
                .content
                .contains("requires approval")
        );
        assert!(!root.join("note.txt").exists());
        let ApprovalNeed::Require(_request) = executor.approval_for(&write) else {
            panic!("write must ask even under permissive policy");
        };
        executor.remember(
            &ApprovalDecision::grant(
                ApprovalRequest::new("other", RiskClass::Mutating, vec![], "{}").id,
            ),
            &write,
        );
        assert!(
            executor
                .invoke(&write, cancel.clone())
                .await
                .content
                .contains("requires approval")
        );
        let ApprovalNeed::Require(request) = executor.approval_for(&write) else {
            panic!("write must ask again");
        };
        executor.remember(&ApprovalDecision::grant(request.id), &write);
        assert!(!executor.invoke(&write, cancel.clone()).await.is_error);
        assert_eq!(
            std::fs::read_to_string(root.join("note.txt")).unwrap(),
            "hello"
        );
        assert!(
            executor
                .invoke(&write, cancel)
                .await
                .content
                .contains("requires approval")
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn destructive_terminal_command_is_blocked_before_prompting() {
        let executor = BoundedExecutor::new(
            ToolRegistry::builtin(),
            PolicyEngine::new(lightagent_core::permissions::ApprovalPolicy::permissive()),
            Duration::from_secs(1),
            1024,
        );
        let destructive = ToolCall {
            id: "remove-1".to_owned(),
            name: "terminal.run".to_owned(),
            arguments: r#"{"command":"rm","args":["-rf","."]}"#.to_owned(),
        };
        assert!(matches!(
            executor.approval_for(&destructive),
            ApprovalNeed::Deny(_)
        ));
    }
}
