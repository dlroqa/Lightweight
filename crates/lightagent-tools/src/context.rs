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

use lightagent_core::{ProfileStore, ProviderFactory, RunId};
use tokio_util::sync::CancellationToken;

use crate::registry::ToolRegistry;

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
}

impl ToolCtx {
    /// A context with no delegation and the system clock.
    pub fn new(cancel: CancellationToken) -> Self {
        Self {
            cancel,
            run: None,
            clock: Clock::System,
            delegation: None,
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
}
