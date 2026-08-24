//! Long operations, watched rather than waited for.
//!
//! Downloading a model is minutes and loading one is tens of seconds on the
//! hardware this targets. A POST that held the socket open for that long would
//! be killed by a proxy, a client timeout or a closed laptop lid, and the work
//! would carry on with nobody to tell — so these return a job id immediately
//! and the caller watches it.
//!
//! Two consequences worth stating:
//!
//! * **The work is not the watching.** A client that goes away does not cancel
//!   a download; it stops receiving updates. Cancelling is a separate,
//!   deliberate act, because a half-finished 1 GB transfer that resumes is
//!   worth more than one abandoned because a browser tab closed.
//! * **A late watcher is not a lost watcher.** Every job keeps its current
//!   state, so a subscriber that arrives after the first updates is told where
//!   things stand before it starts receiving new ones.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use hermes_catalog::InstallProgress;
use hermes_core::JobId;
use hermes_inference::LoadProgress;
use serde::Serialize;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// How many updates a slow subscriber may fall behind before it starts missing
/// them.
///
/// Missing an intermediate progress update is harmless — the next one carries
/// the same information, only newer — and the terminal state is held on the job
/// itself rather than only in the channel, so a subscriber that lags can never
/// miss the outcome.
const UPDATE_BACKLOG: usize = 64;

/// What a job is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Download,
    Import,
    Load,
    Unload,
}

/// Where a job has got to.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum JobState {
    Running {
        stage: Stage,
    },
    /// Finished. `model` is the catalog id it produced, when it produced one.
    Succeeded {
        model: Option<String>,
    },
    Failed {
        error: hermes_core::ErrorReport,
    },
    Cancelled,
}

impl JobState {
    pub const fn is_terminal(&self) -> bool {
        !matches!(self, Self::Running { .. })
    }
}

/// A stage, from whichever operation is running.
///
/// Both progress vocabularies are carried as they are rather than flattened
/// into one: an install reports bytes and a load reports engine lifecycle, and
/// a UI showing a download bar for "starting engine" would be inventing
/// information it does not have.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "of", rename_all = "snake_case")]
pub enum Stage {
    /// Nothing has happened yet.
    Queued,
    Install {
        #[serde(flatten)]
        progress: InstallProgress,
    },
    Load {
        #[serde(flatten)]
        progress: LoadProgress,
    },
}

/// One long operation.
#[derive(Debug)]
pub struct Job {
    pub id: JobId,
    pub kind: JobKind,
    /// Unix seconds.
    pub started_at: u64,
    state: Mutex<JobState>,
    updates: broadcast::Sender<JobState>,
    cancel: CancellationToken,
}

impl Job {
    fn new(kind: JobKind, cancel: CancellationToken) -> Self {
        let (updates, _) = broadcast::channel(UPDATE_BACKLOG);
        Self {
            id: JobId::new(),
            kind,
            started_at: unix_now(),
            state: Mutex::new(JobState::Running {
                stage: Stage::Queued,
            }),
            updates,
            cancel,
        }
    }

    fn lock(&self) -> MutexGuard<'_, JobState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn state(&self) -> JobState {
        self.lock().clone()
    }

    /// Publish a new state. Never blocks: a job must not be slowed by whoever
    /// is watching it, or by nobody watching it at all.
    pub fn set(&self, state: JobState) {
        *self.lock() = state.clone();
        let _ = self.updates.send(state);
    }

    pub fn advance(&self, stage: Stage) {
        self.set(JobState::Running { stage });
    }

    /// The current state, plus every state after it.
    ///
    /// Returned together and under the lock so a job cannot finish in the gap
    /// between reading where it is and subscribing to where it goes — which
    /// would leave a watcher waiting forever for an event that already
    /// happened.
    pub fn watch(&self) -> (JobState, broadcast::Receiver<JobState>) {
        let state = self.lock();
        let receiver = self.updates.subscribe();
        (state.clone(), receiver)
    }

    /// Ask the job to stop. Whether it can is up to the work it is doing.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }
}

/// Every job this process has run.
#[derive(Debug, Default)]
pub struct Jobs {
    inner: Mutex<BTreeMap<u64, Arc<Job>>>,
}

/// How many finished jobs are kept.
///
/// Enough that a UI reopened after a few operations still sees what happened,
/// bounded so a long-running gateway does not accumulate them forever.
const KEEP_FINISHED: usize = 32;

impl Jobs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start tracking a job, rooted in the gateway's shutdown token.
    pub fn start(&self, kind: JobKind, shutdown: &CancellationToken) -> Arc<Job> {
        let job = Arc::new(Job::new(kind, shutdown.child_token()));
        let mut inner = self.lock();
        inner.insert(job.id.get(), Arc::clone(&job));
        prune(&mut inner);
        job
    }

    pub fn get(&self, id: u64) -> Option<Arc<Job>> {
        self.lock().get(&id).map(Arc::clone)
    }

    /// Newest first, which is the order a UI wants them.
    pub fn recent(&self) -> Vec<Arc<Job>> {
        self.lock().values().rev().map(Arc::clone).collect()
    }

    /// Whether anything is still running.
    pub fn any_running(&self) -> bool {
        self.lock().values().any(|job| !job.state().is_terminal())
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<u64, Arc<Job>>> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Drop the oldest finished jobs, never a running one.
fn prune(inner: &mut BTreeMap<u64, Arc<Job>>) {
    let finished: Vec<u64> = inner
        .iter()
        .filter(|(_, job)| job.state().is_terminal())
        .map(|(id, _)| *id)
        .collect();
    let excess = finished.len().saturating_sub(KEEP_FINISHED);
    for id in finished.into_iter().take(excess) {
        inner.remove(&id);
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_job_is_running_and_has_done_nothing() {
        let jobs = Jobs::new();
        let job = jobs.start(JobKind::Download, &CancellationToken::new());
        assert_eq!(
            job.state(),
            JobState::Running {
                stage: Stage::Queued
            }
        );
        assert!(!job.state().is_terminal());
        assert!(jobs.any_running());
    }

    #[tokio::test]
    async fn a_watcher_is_told_where_things_stand_before_what_happens_next() {
        // The race this closes: a job that finishes between "read the state"
        // and "subscribe" would leave the watcher waiting for an event that
        // already happened.
        let jobs = Jobs::new();
        let job = jobs.start(JobKind::Load, &CancellationToken::new());
        job.advance(Stage::Load {
            progress: LoadProgress::LoadingWeights,
        });

        let (current, mut updates) = job.watch();
        assert_eq!(
            current,
            JobState::Running {
                stage: Stage::Load {
                    progress: LoadProgress::LoadingWeights
                }
            }
        );

        job.set(JobState::Succeeded {
            model: Some("qwen3".into()),
        });
        assert_eq!(
            updates.recv().await.expect("an update"),
            JobState::Succeeded {
                model: Some("qwen3".into())
            }
        );
    }

    #[test]
    fn a_job_with_nobody_watching_still_progresses() {
        // `send` on a broadcast channel with no receivers is an error, and
        // treating it as one would mean a download that nobody is watching
        // fails at its first progress update.
        let jobs = Jobs::new();
        let job = jobs.start(JobKind::Download, &CancellationToken::new());
        job.advance(Stage::Install {
            progress: InstallProgress::Downloading {
                downloaded: 1,
                total: Some(2),
            },
        });
        job.set(JobState::Succeeded { model: None });
        assert!(job.state().is_terminal());
        assert!(!jobs.any_running());
    }

    #[test]
    fn finished_jobs_are_kept_for_a_while_and_running_ones_forever() {
        let jobs = Jobs::new();
        let shutdown = CancellationToken::new();

        let running = jobs.start(JobKind::Load, &shutdown);
        for _ in 0..(KEEP_FINISHED + 10) {
            let job = jobs.start(JobKind::Download, &shutdown);
            job.set(JobState::Succeeded { model: None });
        }
        // Pruning happens on insert, so add one more to trigger it.
        jobs.start(JobKind::Download, &shutdown)
            .set(JobState::Succeeded { model: None });

        assert!(
            jobs.get(running.id.get()).is_some(),
            "a running job was pruned"
        );
        assert!(
            jobs.recent().len() <= KEEP_FINISHED + 3,
            "finished jobs accumulate without bound: {}",
            jobs.recent().len()
        );
    }

    #[test]
    fn cancelling_a_job_reaches_the_work_it_is_doing() {
        let jobs = Jobs::new();
        let job = jobs.start(JobKind::Download, &CancellationToken::new());
        let token = job.cancel_token();
        assert!(!token.is_cancelled());
        job.cancel();
        assert!(token.is_cancelled());
    }

    #[test]
    fn a_gateway_shutdown_cancels_every_job_under_it() {
        // Otherwise a download outlives the process that started it.
        let shutdown = CancellationToken::new();
        let jobs = Jobs::new();
        let job = jobs.start(JobKind::Download, &shutdown);
        shutdown.cancel();
        assert!(job.cancel_token().is_cancelled());
    }
}
