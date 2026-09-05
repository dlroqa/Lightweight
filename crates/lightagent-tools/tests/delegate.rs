//! `agent.delegate` starts a fresh, bounded worker run on the same loop.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use lightagent_core::permissions::ApprovalPolicy;
use lightagent_core::{
    AgentEvent, AgentLoop, AgentProfile, AgentProvider, FinishReason, MockProvider, ModelRouting,
    PolicyEngine, ProfileId, ProfileStore, ProviderError, ProviderEvent, ProviderRequest,
    RiskClass, Role, RunConfig, RunId, RunOutcome,
};
use lightagent_tools::{BoundedExecutor, Delegation, ToolRegistry};
use tokio_util::sync::CancellationToken;

/// A factory that hands out clones of one scripted worker provider, so a test
/// can inspect the requests the worker run actually received.
struct MockFactory {
    provider: MockProvider,
}

impl lightagent_core::ProviderFactory for MockFactory {
    fn provider(&self, _routing: &ModelRouting) -> Result<Arc<dyn AgentProvider>, ProviderError> {
        Ok(Arc::new(self.provider.clone()))
    }
}

/// A unique scratch home that is cleaned up on drop.
struct ScratchHome {
    path: PathBuf,
}

impl ScratchHome {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("lightagent-tools-{}", RunId::new().as_str()));
        Self { path }
    }
}

impl Drop for ScratchHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn tool_call_turn(id: &str, name: &str, arguments: &str) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::RoleStarted,
        ProviderEvent::ToolCallDelta {
            index: 0,
            id: Some(id.into()),
            name: Some(name.into()),
            arguments: Some(arguments.into()),
        },
        ProviderEvent::Finished {
            reason: FinishReason::ToolCalls,
            usage: None,
        },
    ]
}

fn final_turn(text: &str) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::Content(text.into()),
        ProviderEvent::Finished {
            reason: FinishReason::Stop,
            usage: None,
        },
    ]
}

/// Scaffold a `worker` profile with a persona, in a scratch home.
fn worker_store(home: &std::path::Path) -> ProfileStore {
    let store = ProfileStore::new(home.to_path_buf());
    let id = ProfileId::new("worker").unwrap();
    let profile = AgentProfile::new(id, "Worker", "You are a calculator.", "worker-model");
    store.save(&profile).unwrap();
    store
}

fn delegation(store: ProfileStore, worker: MockProvider) -> Delegation {
    Delegation {
        profiles: Arc::new(store),
        factory: Arc::new(MockFactory { provider: worker }),
        worker_registry: ToolRegistry::worker_default(),
        worker_per_call: Duration::from_secs(5),
        worker_max_output_bytes: 262_144,
    }
}

#[tokio::test]
async fn delegate_runs_a_worker_with_fresh_context_and_returns_its_answer() {
    let home = ScratchHome::new();
    let store = worker_store(&home.path);
    let worker_provider = MockProvider::new(vec![final_turn("4")]);

    let orchestrator = MockProvider::new(vec![
        tool_call_turn(
            "call_1",
            "agent.delegate",
            r#"{"profile":"worker","task":"add 2+2"}"#,
        ),
        final_turn("The worker computed it."),
    ]);
    let executor = BoundedExecutor::new(
        ToolRegistry::builtin(),
        PolicyEngine::new(ApprovalPolicy::permissive()),
        Duration::from_secs(5),
        262_144,
    )
    .with_run(RunId::new())
    .with_delegation(delegation(store, worker_provider.clone()));

    let agent = AgentLoop::new(orchestrator, executor, RunConfig::new("orchestrator-model"));
    let outcome = agent
        .run("delegate please", CancellationToken::new())
        .await
        .unwrap();
    let events = outcome.into_events();

    // The delegate tool ran and returned the worker's answer.
    let delegated = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ToolCallCompleted { outcome, .. } => Some(outcome),
            _ => None,
        })
        .expect("the delegate tool completed");
    assert!(!delegated.is_error, "delegation succeeded: {delegated:?}");
    assert_eq!(delegated.content, "4");

    // The worker ran with a FRESH context: its first request is exactly the
    // worker's persona as system and the task as the user turn — none of the
    // orchestrator's history.
    let worker_requests = worker_provider.requests();
    let first: &ProviderRequest = worker_requests.first().expect("the worker was called");
    assert_eq!(first.messages.len(), 2);
    assert_eq!(first.messages[0].role, Role::System);
    assert_eq!(first.messages[0].content, "You are a calculator.");
    assert_eq!(first.messages[1].role, Role::User);
    assert_eq!(first.messages[1].content, "add 2+2");
    assert_eq!(first.model, "worker-model");
}

#[tokio::test]
async fn a_worker_is_offered_no_delegate_tool() {
    // Single level: the registry a worker run is given never contains
    // agent.delegate, so a worker cannot delegate again.
    let registry = ToolRegistry::worker_default();
    assert!(!registry.contains("agent.delegate"));
    assert!(registry.contains("datetime.now"));
}

#[tokio::test]
async fn delegate_requires_approval_under_a_balanced_policy() {
    let home = ScratchHome::new();
    let store = worker_store(&home.path);
    let worker_provider = MockProvider::new(vec![final_turn("4")]);

    let orchestrator = MockProvider::new(vec![tool_call_turn(
        "call_1",
        "agent.delegate",
        r#"{"profile":"worker","task":"add 2+2"}"#,
    )]);
    let executor = BoundedExecutor::new(
        ToolRegistry::builtin(),
        PolicyEngine::new(ApprovalPolicy::balanced()),
        Duration::from_secs(5),
        262_144,
    )
    .with_run(RunId::new())
    .with_delegation(delegation(store, worker_provider));

    let agent = AgentLoop::new(orchestrator, executor, RunConfig::new("orchestrator-model"));
    let outcome = agent
        .run("delegate please", CancellationToken::new())
        .await
        .unwrap();

    let RunOutcome::AwaitingApproval { request, .. } = outcome else {
        panic!("an Executable delegate must pause for approval under a balanced policy");
    };
    assert_eq!(request.tool, "agent.delegate");
    assert_eq!(request.risk, RiskClass::Executable);
}

#[tokio::test]
async fn delegate_errors_when_delegation_is_not_enabled() {
    // No `.with_delegation`, so the tool has nothing to run a worker with.
    let orchestrator = MockProvider::new(vec![
        tool_call_turn(
            "call_1",
            "agent.delegate",
            r#"{"profile":"worker","task":"x"}"#,
        ),
        final_turn("done"),
    ]);
    let executor = BoundedExecutor::new(
        ToolRegistry::builtin(),
        PolicyEngine::new(ApprovalPolicy::permissive()),
        Duration::from_secs(5),
        262_144,
    );
    let agent = AgentLoop::new(orchestrator, executor, RunConfig::new("orchestrator-model"));
    let events = agent
        .run("delegate please", CancellationToken::new())
        .await
        .unwrap()
        .into_events();

    let outcome = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ToolCallCompleted { outcome, .. } => Some(outcome),
            _ => None,
        })
        .expect("a controlled result");
    assert!(outcome.is_error);
    assert!(outcome.content.contains("delegation is not enabled"));
}
