//! A scripted provider for deterministic, offline tests.
//!
//! [`MockProvider`] hands back a pre-scripted list of [`ProviderEvent`]s per
//! turn and records every [`ProviderRequest`] it was given, so a test can drive
//! the loop through a whole exchange and then assert both the events it emitted
//! and the messages it sent. It performs no I/O and no waiting, so a test runs
//! at memory speed.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures_util::stream::{self, StreamExt};
use tokio_util::sync::CancellationToken;

use crate::provider::{
    AgentProvider, ProviderError, ProviderEvent, ProviderRequest, ProviderStream,
};

/// A provider whose turns are scripted ahead of time.
#[derive(Clone)]
pub struct MockProvider {
    inner: Arc<Inner>,
}

struct Inner {
    scripts: Vec<Vec<ProviderEvent>>,
    /// When true, once the scripts are exhausted the last one repeats forever —
    /// which is how a "the model keeps asking for tools" scenario is driven to
    /// a limit rather than to a natural end.
    repeat_last: bool,
    cursor: AtomicUsize,
    requests: Mutex<Vec<ProviderRequest>>,
}

impl MockProvider {
    /// A provider that plays each script in turn, then yields an empty stream.
    pub fn new(scripts: Vec<Vec<ProviderEvent>>) -> Self {
        Self::build(scripts, false)
    }

    /// A provider that repeats its final script once the list is exhausted.
    pub fn looping(scripts: Vec<Vec<ProviderEvent>>) -> Self {
        Self::build(scripts, true)
    }

    fn build(scripts: Vec<Vec<ProviderEvent>>, repeat_last: bool) -> Self {
        Self {
            inner: Arc::new(Inner {
                scripts,
                repeat_last,
                cursor: AtomicUsize::new(0),
                requests: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Every request the loop has sent so far.
    pub fn requests(&self) -> Vec<ProviderRequest> {
        self.inner
            .requests
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }
}

#[async_trait]
impl AgentProvider for MockProvider {
    async fn stream(
        &self,
        request: ProviderRequest,
        _cancel: CancellationToken,
    ) -> Result<ProviderStream, ProviderError> {
        if let Ok(mut requests) = self.inner.requests.lock() {
            requests.push(request);
        }

        let turn = self.inner.cursor.fetch_add(1, Ordering::Relaxed);
        let script = self.inner.scripts.get(turn).cloned().or_else(|| {
            if self.inner.repeat_last {
                self.inner.scripts.last().cloned()
            } else {
                None
            }
        });

        let events: VecDeque<Result<ProviderEvent, ProviderError>> =
            script.unwrap_or_default().into_iter().map(Ok).collect();
        Ok(stream::iter(events).boxed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{FinishReason, ProviderRequest};

    #[tokio::test]
    async fn scripts_play_in_order_and_requests_are_recorded() {
        let mock = MockProvider::new(vec![
            vec![
                ProviderEvent::Content("hi".into()),
                ProviderEvent::Finished {
                    reason: FinishReason::Stop,
                    usage: None,
                },
            ],
            vec![ProviderEvent::Finished {
                reason: FinishReason::Stop,
                usage: None,
            }],
        ]);

        let first: Vec<_> = mock
            .stream(
                ProviderRequest::new("m", Vec::new()),
                CancellationToken::new(),
            )
            .await
            .expect("stream")
            .collect()
            .await;
        assert_eq!(first.len(), 2);

        let _ = mock
            .stream(
                ProviderRequest::new("m", Vec::new()),
                CancellationToken::new(),
            )
            .await
            .expect("stream")
            .collect::<Vec<_>>()
            .await;

        assert_eq!(mock.requests().len(), 2);
    }
}
