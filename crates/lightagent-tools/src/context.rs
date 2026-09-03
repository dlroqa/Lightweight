//! What a tool is given when it runs.
//!
//! A [`ToolCtx`] carries the cancellation token, the id of the run the call
//! belongs to (so a delegated child can name its parent), a [`Clock`] a tool
//! reads instead of the wall clock (which is what makes `datetime.now`
//! deterministic under test), and — only when delegation is enabled — a
//! [`Delegation`] bundle with the worker profile store, the provider factory and
//! the registry and bounds a worker run is given.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use lightagent_core::{ProfileStore, ProviderFactory, RunId, SkillStore};
use tokio_util::sync::CancellationToken;

use crate::registry::ToolRegistry;
use crate::workspace::Workspace;

/// The time source a tool reads.
///
/// `System` reads the real clock; `Fixed` returns a pinned instant, so a test
/// asserts an exact rendering rather than a moving one.
#[derive(Clone, Debug, Default)]
pub enum Clock {
    /// The real wall clock.
    #[default]
    System,
    /// A pinned instant, for deterministic tests.
    Fixed(SystemTime),
}

impl Clock {
    /// The current instant according to this clock.
    pub fn now(&self) -> SystemTime {
        match self {
            Self::System => SystemTime::now(),
            Self::Fixed(instant) => *instant,
        }
    }
}

/// Everything `agent.delegate` needs to start a worker run, injected by the
/// caller so this crate never depends on a transport.
#[derive(Clone)]
pub struct Delegation {
    /// Where worker profiles ("bots") are loaded from.
    pub profiles: Arc<ProfileStore>,
    /// Maps a worker's routing to a provider without this crate knowing how.
    pub factory: Arc<dyn ProviderFactory>,
    /// The tools a worker run may use. `agent.delegate` is absent from it, so a
    /// worker cannot delegate again — delegation is one level deep this pass.
    pub worker_registry: ToolRegistry,
    /// The per-call timeout a worker run's executor enforces.
    pub worker_per_call: Duration,
    /// The output ceiling a worker run's executor enforces.
    pub worker_max_output_bytes: usize,
}

/// The effective web-access settings a web tool enforces.
///
/// A plain, resolved view — the caller translates the persisted `WebConfig` into
/// this once (resolving the search key's [`SecretRef`](lightagent_core::SecretRef)
/// to a value held only in memory), so this crate never learns the config format
/// and never re-reads a secret. `allow_domains`, when non-empty, is both a fetch
/// allow-list and the set of hosts exempt from the private-address guard.
#[derive(Clone, Debug)]
pub struct WebPolicy {
    /// When non-empty, a fetch host must match one of these (exactly or as a
    /// subdomain); such a host is also exempt from `block_private_addresses`.
    pub allow_domains: Vec<String>,
    /// Refuse a fetch whose host resolves to a non-global address.
    pub block_private_addresses: bool,
    /// The most bytes a single fetch reads before truncating.
    pub max_fetch_bytes: usize,
    /// The per-request timeout.
    pub timeout: Duration,
    /// The `web.search` endpoint; `None` means no backend is configured.
    pub search_endpoint: Option<String>,
    /// The query-string parameter the search endpoint reads the query from.
    pub search_query_param: String,
    /// The resolved search bearer key, held only in memory; never logged.
    pub search_api_key: Option<String>,
    /// The most results a single search returns.
    pub search_max_results: usize,
}

/// Everything the web tools need to run, injected by the caller so this crate
/// never builds an HTTP client or reads the config format itself.
#[derive(Clone)]
pub struct WebContext {
    /// The HTTP client the caller built (with the rustls provider installed and
    /// automatic redirects disabled, so the tool follows them under its guard).
    pub client: reqwest::Client,
    /// The effective web policy.
    pub policy: Arc<WebPolicy>,
}

/// The effective filesystem/terminal settings the workspace tools enforce.
///
/// A resolved view the caller builds once from the persisted `ToolsConfig`, so
/// this crate never learns the config format. `allow_terminal` gates
/// `terminal.run` on top of the workspace being present at all.
#[derive(Clone, Debug)]
pub struct WorkspacePolicy {
    /// The most bytes a single `fs.read`/`fs.write` may move.
    pub max_file_bytes: usize,
    /// Whether `terminal.run` may run at all.
    pub allow_terminal: bool,
    /// The wall-clock timeout for one `terminal.run`.
    pub terminal_timeout: Duration,
    /// When non-empty, the only program names `terminal.run` may launch.
    pub terminal_allowlist: Vec<String>,
}

/// Everything the filesystem and terminal tools need, injected by the caller so
/// this crate never resolves the workspace path or reads the config itself.
#[derive(Clone)]
pub struct WorkspaceContext {
    /// The confined root every path is resolved through.
    pub workspace: Arc<Workspace>,
    /// The effective policy.
    pub policy: Arc<WorkspacePolicy>,
}

/// The skills a run's `skill.read` tool can serve, injected by the caller so this
/// crate does not read the skills directories itself.
#[derive(Clone)]
pub struct SkillContext {
    /// The loaded skills.
    pub skills: Arc<SkillStore>,
}

/// The context handed to a [`Tool`](crate::Tool) when it runs.
#[derive(Clone)]
pub struct ToolCtx {
    /// Cancelled when the run is torn down; a long tool should stop.
    pub cancel: CancellationToken,
    /// The run this call belongs to, named as the `parent` of any child run the
    /// call starts. `None` when the caller did not supply one.
    pub run: Option<RunId>,
    /// The time source, so time-reading tools are testable.
    pub clock: Clock,
    /// Present only when delegation is enabled for this run.
    pub delegation: Option<Delegation>,
    /// Present only when web access is enabled for this run.
    pub web: Option<WebContext>,
    /// Present only when filesystem/terminal access is enabled for this run.
    pub workspace: Option<WorkspaceContext>,
    /// Present only when skills are available for this run.
    pub skills: Option<SkillContext>,
}

impl ToolCtx {
    /// A context with no delegation, no web access and the system clock.
    pub fn new(cancel: CancellationToken) -> Self {
        Self {
            cancel,
            run: None,
            clock: Clock::System,
            delegation: None,
            web: None,
            workspace: None,
            skills: None,
        }
    }

    /// Set the owning run id (named as a child's parent).
    pub fn with_run(mut self, run: RunId) -> Self {
        self.run = Some(run);
        self
    }

    /// Set the clock.
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// Enable delegation with the given bundle.
    pub fn with_delegation(mut self, delegation: Delegation) -> Self {
        self.delegation = Some(delegation);
        self
    }

    /// Enable web access with the given context.
    pub fn with_web(mut self, web: WebContext) -> Self {
        self.web = Some(web);
        self
    }

    /// Enable filesystem/terminal access with the given context.
    pub fn with_workspace(mut self, workspace: WorkspaceContext) -> Self {
        self.workspace = Some(workspace);
        self
    }

    /// Make skills available to the run's `skill.read` tool.
    pub fn with_skills(mut self, skills: SkillContext) -> Self {
        self.skills = Some(skills);
        self
    }
}
