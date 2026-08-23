//! What every request handler shares.

use std::sync::Arc;

use hermes_inference::InferenceBackend;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::auth::AuthPolicy;
use crate::catalog::Catalog;

/// How the gateway behaves.
#[derive(Clone, Debug)]
pub struct GatewayConfig {
    pub auth: AuthPolicy,
    /// Requests that may run at once.
    ///
    /// One, because the engine serves one and because a second concurrent
    /// generation on a four-core CPU makes both slower than running them in
    /// turn. The number is a *parameter* rather than a constant so that
    /// continuous batching later is configuration and not a rewrite: raise it,
    /// pass `--parallel N` to the engine, and the KV formula already carries
    /// the factor.
    pub max_concurrent_requests: u32,
    /// How long a request may wait for its turn before the client is told to
    /// come back.
    ///
    /// Prefill and decode on this hardware are slow enough that a queued
    /// request can wait minutes; the alternative to waiting is a 503 that
    /// makes the client retry into the same queue.
    pub queue_timeout: std::time::Duration,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            auth: AuthPolicy::Disabled,
            max_concurrent_requests: 1,
            queue_timeout: std::time::Duration::from_secs(600),
        }
    }
}

/// Shared handler state.
pub struct GatewayState {
    pub backend: Arc<dyn InferenceBackend>,
    pub catalog: Arc<Catalog>,
    pub config: GatewayConfig,
    /// One permit per concurrent request.
    slots: Arc<Semaphore>,
    /// Cancelled when the gateway shuts down.
    ///
    /// The root of the cancellation tree: shutdown cancels every job, and each
    /// job's own token is a child, so nothing can outlive the process it
    /// belongs to.
    shutdown: CancellationToken,
}

impl std::fmt::Debug for GatewayState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayState")
            .field("backend", &self.backend.id())
            .field("config", &self.config)
            .field("available_slots", &self.slots.available_permits())
            .finish()
    }
}

impl GatewayState {
    pub fn new(
        backend: Arc<dyn InferenceBackend>,
        catalog: Arc<Catalog>,
        config: GatewayConfig,
    ) -> Self {
        let slots = Arc::new(Semaphore::new(
            config.max_concurrent_requests.max(1) as usize
        ));
        Self {
            backend,
            catalog,
            config,
            slots,
            shutdown: CancellationToken::new(),
        }
    }

    /// A permit to run one request, waited for up to the queue timeout.
    ///
    /// Returns `None` when the wait ran out, which the caller turns into a 503
    /// with a `Retry-After` rather than a request that hangs forever.
    pub async fn acquire_slot(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        tokio::time::timeout(
            self.config.queue_timeout,
            Arc::clone(&self.slots).acquire_owned(),
        )
        .await
        .ok()?
        .ok()
    }

    /// A cancellation token for one job, rooted in the gateway's own.
    ///
    /// Child rather than independent: a shutdown must reach an in-flight
    /// generation, or the process waits on an engine nobody is listening to.
    pub fn job_token(&self) -> CancellationToken {
        self.shutdown.child_token()
    }

    /// The token that cancels everything.
    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    /// Stop accepting work and cancel what is running.
    pub fn shutdown(&self) {
        self.shutdown.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_backend_mock::MockBackend;

    fn state(config: GatewayConfig) -> GatewayState {
        GatewayState::new(
            Arc::new(MockBackend::default()),
            crate::catalog::shared(None),
            config,
        )
    }

    #[tokio::test]
    async fn one_request_runs_at_a_time_by_default() {
        let state = state(GatewayConfig::default());
        let first = state.acquire_slot().await.expect("first permit");
        assert_eq!(state.slots.available_permits(), 0);
        drop(first);
        assert_eq!(state.slots.available_permits(), 1);
    }

    #[tokio::test]
    async fn a_queued_request_gives_up_rather_than_hanging_forever() {
        let state = state(GatewayConfig {
            queue_timeout: std::time::Duration::from_millis(20),
            ..GatewayConfig::default()
        });
        let _held = state.acquire_slot().await.expect("first permit");
        assert!(
            state.acquire_slot().await.is_none(),
            "the second request must time out rather than wait forever"
        );
    }

    #[tokio::test]
    async fn a_shutdown_cancels_jobs_that_are_already_running() {
        // Otherwise the process waits on a generation whose client is gone.
        let state = state(GatewayConfig::default());
        let job = state.job_token();
        assert!(!job.is_cancelled());
        state.shutdown();
        assert!(job.is_cancelled());
    }

    #[tokio::test]
    async fn raising_the_concurrency_is_configuration_not_a_rewrite() {
        // The whole point of keeping this a parameter: continuous batching
        // later must not need a new type or a new caller.
        let state = state(GatewayConfig {
            max_concurrent_requests: 4,
            ..GatewayConfig::default()
        });
        let mut permits = Vec::new();
        for _ in 0..4 {
            permits.push(state.acquire_slot().await.expect("permit"));
        }
        assert_eq!(state.slots.available_permits(), 0);
    }
}
