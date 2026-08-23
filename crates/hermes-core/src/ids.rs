//! Identifiers.
//!
//! [`ModelId`] carries a naming policy that exists for one specific reason, so
//! it is worth stating up front. Hermes caches a model's context length under
//! the key `f"{model}@{base_url}"` in `~/.hermes/context_length_cache.yaml`
//! (verified in `agent/model_metadata.py:1466-1567`). If we serve
//! `llama-3.2-3b` at 8192 tokens, Hermes caches 8192 against that name — and if
//! the user later reloads the same model at 32768, the cached entry is stale
//! and Hermes keeps sizing prompts to the old number.
//!
//! Encoding the effective context into the id (`llama-3.2-3b@8k`) makes a
//! context change produce a *different cache key*, so the stale entry simply
//! becomes unreachable instead of becoming wrong. That is the entire reason
//! [`ModelId::with_context`] exists.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// A model identifier as it appears in the OpenAI API's `model` field.
///
/// By convention this is `{slug}@{context}`, for example
/// `lfm2.5-1.2b-q4_k_m@8k`, though a bare slug is also valid — models are
/// looked up by the whole string, so an id without a context suffix resolves
/// perfectly well.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelId(String);

impl ModelId {
    /// Build an id from a slug with no context suffix.
    pub fn new(slug: impl Into<String>) -> Self {
        Self(slug.into())
    }

    /// Build `{slug}@{context}`.
    ///
    /// The context renders as `8k` when it is an exact multiple of 1024 and as
    /// the raw token count otherwise, so the suffix is always exact — never
    /// rounded, which would make two different context sizes collide on one
    /// cache key and reintroduce the staleness this is here to prevent.
    pub fn with_context(slug: impl AsRef<str>, n_ctx: u32) -> Self {
        Self(format!("{}@{}", slug.as_ref(), Self::format_context(n_ctx)))
    }

    fn format_context(n_ctx: u32) -> String {
        if n_ctx >= 1024 && n_ctx.is_multiple_of(1024) {
            format!("{}k", n_ctx / 1024)
        } else {
            n_ctx.to_string()
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The portion before the context suffix, or the whole id if there is none.
    pub fn slug(&self) -> &str {
        match self.0.rsplit_once('@') {
            Some((slug, suffix)) if Self::parse_context(suffix).is_some() => slug,
            _ => &self.0,
        }
    }

    /// The effective context encoded in the id, if it has a valid suffix.
    pub fn context(&self) -> Option<u32> {
        let (_, suffix) = self.0.rsplit_once('@')?;
        Self::parse_context(suffix)
    }

    fn parse_context(suffix: &str) -> Option<u32> {
        if let Some(kibi) = suffix.strip_suffix('k') {
            kibi.parse::<u32>().ok()?.checked_mul(1024)
        } else {
            suffix.parse::<u32>().ok()
        }
        .filter(|&tokens| tokens > 0)
    }
}

impl fmt::Display for ModelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for ModelId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for ModelId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

/// Monotonic source for the process-local identifiers below.
///
/// Deliberately not random: these never leave the machine, and a counter makes
/// logs from a single run readable in order. Anything that *does* leave the
/// machine (an SSE `id`, a tool-call `id`) is generated where it is needed and
/// is not this type.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Identifies one loaded instance of a model inside a backend.
///
/// Distinct from [`ModelId`]: unloading and reloading the same model yields a
/// new `InstanceId`, which is what lets an in-flight request notice that the
/// instance it was queued against no longer exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InstanceId(u64);

impl InstanceId {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self(next_id())
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for InstanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "instance-{}", self.0)
    }
}

/// Identifies one unit of work in the scheduler, from submission to completion.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct JobId(u64);

impl JobId {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self(next_id())
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "job-{}", self.0)
    }
}

/// Identifies the caller, for scheduler fairness and per-client metrics.
///
/// The scheduler round-robins between clients within a priority band so that
/// the desktop UI and Hermes cannot starve one another. That only works if
/// requests can be attributed, hence this.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ClientKey {
    /// The bundled desktop UI, over the private control API.
    LocalUi,
    /// An API caller identified by which configured key it presented.
    ApiKey(String),
    /// An unauthenticated caller on the loopback interface, identified by peer
    /// address. This is the usual case for Hermes, which sends
    /// `Bearer no-key-required` when no key is configured.
    Anonymous(String),
}

impl fmt::Display for ClientKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LocalUi => f.write_str("local-ui"),
            Self::ApiKey(name) => write!(f, "key:{name}"),
            Self::Anonymous(peer) => write!(f, "anon:{peer}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_suffix_uses_k_for_exact_multiples() {
        assert_eq!(
            ModelId::with_context("llama-3.2-3b", 8192).as_str(),
            "llama-3.2-3b@8k"
        );
        assert_eq!(
            ModelId::with_context("llama-3.2-3b", 131072).as_str(),
            "llama-3.2-3b@128k"
        );
    }

    #[test]
    fn context_suffix_stays_exact_for_odd_values() {
        // Rounding 8000 to "8k" would make 8000 and 8192 share a cache key in
        // Hermes, which is precisely the collision this naming exists to avoid.
        assert_eq!(ModelId::with_context("m", 8000).as_str(), "m@8000");
    }

    #[test]
    fn round_trips_slug_and_context() {
        let id = ModelId::with_context("lfm2.5-1.2b-q4_k_m", 8192);
        assert_eq!(id.slug(), "lfm2.5-1.2b-q4_k_m");
        assert_eq!(id.context(), Some(8192));
    }

    #[test]
    fn changing_context_changes_the_identity() {
        // The whole point: Hermes keys its context cache on the model name, so
        // two context sizes must never share one name.
        let at_8k = ModelId::with_context("m", 8192);
        let at_32k = ModelId::with_context("m", 32768);
        assert_ne!(at_8k, at_32k);
    }

    #[test]
    fn bare_slug_has_no_context() {
        let id = ModelId::new("llama-3.2-3b");
        assert_eq!(id.slug(), "llama-3.2-3b");
        assert_eq!(id.context(), None);
    }

    #[test]
    fn an_at_sign_that_is_not_a_context_is_left_alone() {
        // Model ids from other sources can contain '@' — a HuggingFace revision
        // pin, for instance. Only a parseable token count counts as a suffix.
        let id = ModelId::new("org/model@main");
        assert_eq!(id.slug(), "org/model@main");
        assert_eq!(id.context(), None);
    }

    #[test]
    fn rejects_a_zero_context_suffix() {
        assert_eq!(ModelId::new("m@0").context(), None);
        assert_eq!(ModelId::new("m@0").slug(), "m@0");
    }

    #[test]
    fn ids_are_unique_and_ordered() {
        let first = JobId::new();
        let second = JobId::new();
        assert!(second.get() > first.get());
    }

    #[test]
    fn instance_and_job_ids_display_distinguishably() {
        assert!(InstanceId::new().to_string().starts_with("instance-"));
        assert!(JobId::new().to_string().starts_with("job-"));
    }

    #[test]
    fn model_id_serializes_as_a_bare_string() {
        let json = serde_json::to_string(&ModelId::new("m@8k")).expect("serialize");
        assert_eq!(json, "\"m@8k\"");
    }
}
