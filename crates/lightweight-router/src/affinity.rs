//! Session affinity: keeping one conversation on one deployment.
//!
//! A client that names its session — in the header the configuration chooses,
//! `X-Lightweight-Session` by default — has its next request on the same route
//! **preferred** to go where its last one succeeded. Preferred, never forced:
//! the affinity is consulted only after health and the request's own
//! requirements have decided which deployments may take it (see
//! [`crate::select::Selector::plan_with_affinity`]). A sticky deployment that
//! is down, disabled, swapped to another model, or unable to serve this
//! request is passed over exactly as if there were no session at all, and the
//! session moves to wherever the request then succeeds.
//!
//! What is stored, and what is not:
//!
//! * The key is the route and a **keyed hash** of the session id. The raw id
//!   is never held: it is hashed on arrival with a key drawn at startup, so
//!   the map, the admin view and the traces cannot be read back into the ids
//!   clients sent, nor matched across restarts.
//! * The value is a deployment id and two instants. No prompt, no message, no
//!   token, no address, no credential.
//! * Sessions are never inferred. No header, no affinity: an IP address, an
//!   API key or a user agent is not a conversation, and treating one as such
//!   would turn an operational hint into tracking.
//!
//! Bounded on both axes: an entry idle for longer than the TTL is gone
//! (removed when next looked up, and by a periodic sweep), and the map never
//! holds more than `max_entries` — when full, expired entries go first and
//! then the least recently used. Memory only; a restart forgets every
//! affinity, which costs one request of re-selection per session.
//!
//! Two hashes colliding (one in 2^64 per pair) would make two sessions share
//! a preference — harmless, because the preference is only ever among
//! deployments that were valid anyway.

use std::collections::HashMap;
use std::hash::{BuildHasher, RandomState};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::http::HeaderMap;

use crate::config::AffinityPolicy;
use crate::domain::{DeploymentId, RouteName};

/// The longest session id the router will read. Longer ones are ignored, as
/// if absent, rather than truncated into a different session.
pub const MAX_SESSION_ID: usize = 256;

/// Which affinity a request belongs to. Opaque: it holds the route and a keyed
/// hash, never the id the client sent.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AffinityKey {
    route: String,
    session: u64,
}

impl AffinityKey {
    pub fn route(&self) -> &str {
        &self.route
    }

    /// Eight hex digits of the keyed hash: enough to tell sessions apart in a
    /// trace or the admin view, useless for recovering the id.
    pub fn fingerprint(&self) -> String {
        format!("{:08x}", self.session >> 32)
    }
}

#[derive(Clone, Debug)]
struct Entry {
    deployment: DeploymentId,
    created: Instant,
    last_used: Instant,
}

/// What [`AffinityBook::establish`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Established {
    /// No affinity existed; this deployment is now the session's.
    New,
    /// The session already pointed here.
    Unchanged,
    /// A concurrent request established the session first, elsewhere. The
    /// first commit wins; this one's deployment is not recorded.
    KeptExisting(DeploymentId),
}

/// One entry as the admin view shows it.
#[derive(Clone, Debug, serde::Serialize)]
pub struct EntryView {
    pub route: String,
    pub session: String,
    pub deployment: DeploymentId,
    pub age_secs: u64,
    pub idle_secs: u64,
}

/// Why entries left the map, for the metrics.
#[derive(Debug, Default)]
struct Evictions {
    expired: AtomicU64,
    capacity: AtomicU64,
}

/// Every live affinity, behind one short lock.
///
/// The lock is held for a hash-map operation and nothing else — never across
/// a network call — so sessions do not wait on each other in any way a
/// request could notice.
#[derive(Debug)]
pub struct AffinityBook {
    policy: AffinityPolicy,
    hasher: RandomState,
    entries: Mutex<HashMap<AffinityKey, Entry>>,
    evictions: Evictions,
}

impl AffinityBook {
    pub fn new(policy: AffinityPolicy) -> Self {
        Self {
            policy,
            hasher: RandomState::new(),
            entries: Mutex::new(HashMap::new()),
            evictions: Evictions::default(),
        }
    }

    pub fn policy(&self) -> &AffinityPolicy {
        &self.policy
    }

    pub fn enabled(&self) -> bool {
        self.policy.enabled
    }

    /// The affinity this request belongs to: `None` when affinity is off, or
    /// the request names no usable session.
    pub fn session(&self, route: &RouteName, headers: &HeaderMap) -> Option<AffinityKey> {
        if !self.policy.enabled {
            return None;
        }
        let id = headers
            .get(self.policy.header.as_str())
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|id| usable(id))?;
        Some(self.key(route, id))
    }

    /// The key for `session` on `route`.
    pub fn key(&self, route: &RouteName, session: &str) -> AffinityKey {
        AffinityKey {
            route: route.as_str().to_owned(),
            session: self.hasher.hash_one(session),
        }
    }

    /// The deployment this session last succeeded on, if its affinity has not
    /// expired. An expired entry is removed here.
    pub fn lookup(&self, key: &AffinityKey) -> Option<DeploymentId> {
        self.lookup_at(key, Instant::now())
    }

    pub fn lookup_at(&self, key: &AffinityKey, now: Instant) -> Option<DeploymentId> {
        let mut entries = self.lock();
        let entry = entries.get_mut(key)?;
        if self.expired(entry, now) {
            entries.remove(key);
            self.evictions.expired.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        // Looking it up is using it: a long stream must not let its own
        // session expire under it.
        entry.last_used = now;
        Some(entry.deployment.clone())
    }

    /// Record the first successful deployment of a session that had no
    /// affinity when its request was planned.
    ///
    /// If a concurrent request for the same session committed first, its
    /// choice stands: the map is never torn, and the session settles on one
    /// deployment rather than flipping between the racers.
    pub fn establish(&self, key: AffinityKey, deployment: &DeploymentId) -> Established {
        self.establish_at(key, deployment, Instant::now())
    }

    pub fn establish_at(
        &self,
        key: AffinityKey,
        deployment: &DeploymentId,
        now: Instant,
    ) -> Established {
        let mut entries = self.lock();
        if let Some(entry) = entries.get_mut(&key)
            && !self.expired(entry, now)
        {
            entry.last_used = now;
            return if &entry.deployment == deployment {
                Established::Unchanged
            } else {
                Established::KeptExisting(entry.deployment.clone())
            };
        }
        self.insert(&mut entries, key, deployment, now);
        Established::New
    }

    /// Move a session to the deployment that just served it, because its
    /// previous one could not.
    pub fn reassign(&self, key: AffinityKey, deployment: &DeploymentId) {
        self.reassign_at(key, deployment, Instant::now());
    }

    pub fn reassign_at(&self, key: AffinityKey, deployment: &DeploymentId, now: Instant) {
        let mut entries = self.lock();
        self.insert(&mut entries, key, deployment, now);
    }

    /// Remove every expired entry. Returns how many went.
    pub fn sweep(&self) -> usize {
        self.sweep_at(Instant::now())
    }

    pub fn sweep_at(&self, now: Instant) -> usize {
        let mut entries = self.lock();
        let before = entries.len();
        entries.retain(|_, entry| !self.expired(entry, now));
        let removed = before - entries.len();
        self.evictions
            .expired
            .fetch_add(removed as u64, Ordering::Relaxed);
        removed
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Entries removed for idling past the TTL, and to make room.
    pub fn evictions(&self) -> (u64, u64) {
        (
            self.evictions.expired.load(Ordering::Relaxed),
            self.evictions.capacity.load(Ordering::Relaxed),
        )
    }

    /// Every live entry, most recently used first, for the admin view.
    pub fn entries(&self) -> Vec<EntryView> {
        let now = Instant::now();
        let entries = self.lock();
        let mut rows: Vec<(Instant, EntryView)> = entries
            .iter()
            .filter(|(_, entry)| !self.expired(entry, now))
            .map(|(key, entry)| {
                (
                    entry.last_used,
                    EntryView {
                        route: key.route.clone(),
                        session: key.fingerprint(),
                        deployment: entry.deployment.clone(),
                        age_secs: now.saturating_duration_since(entry.created).as_secs(),
                        idle_secs: now.saturating_duration_since(entry.last_used).as_secs(),
                    },
                )
            })
            .collect();
        rows.sort_by(|(a, _), (b, _)| b.cmp(a));
        rows.into_iter().map(|(_, row)| row).collect()
    }

    /// How often the background sweep should run: often enough that an
    /// expired entry does not outlive its TTL by much, rarely enough to cost
    /// nothing.
    pub fn sweep_interval(&self) -> Duration {
        self.policy
            .idle_ttl
            .clamp(Duration::from_secs(1), Duration::from_secs(60))
    }

    fn insert(
        &self,
        entries: &mut HashMap<AffinityKey, Entry>,
        key: AffinityKey,
        deployment: &DeploymentId,
        now: Instant,
    ) {
        if let Some(entry) = entries.get_mut(&key) {
            let created = if entry.deployment == *deployment && !self.expired(entry, now) {
                entry.created
            } else {
                now
            };
            *entry = Entry {
                deployment: deployment.clone(),
                created,
                last_used: now,
            };
            return;
        }
        if entries.len() >= self.policy.max_entries {
            // Expired entries first: they are already gone in all but name.
            let before = entries.len();
            entries.retain(|_, entry| !self.expired(entry, now));
            self.evictions
                .expired
                .fetch_add((before - entries.len()) as u64, Ordering::Relaxed);
        }
        while entries.len() >= self.policy.max_entries {
            // Then the least recently used. A linear scan, reached only when
            // the map is full of live sessions; simpler than an ordered index
            // kept beside the map on every request.
            let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            entries.remove(&oldest);
            self.evictions.capacity.fetch_add(1, Ordering::Relaxed);
        }
        entries.insert(
            key,
            Entry {
                deployment: deployment.clone(),
                created: now,
                last_used: now,
            },
        );
    }

    fn expired(&self, entry: &Entry, now: Instant) -> bool {
        now.saturating_duration_since(entry.last_used) > self.policy.idle_ttl
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<AffinityKey, Entry>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A session id the router will read: visible ASCII, not empty, not
/// unreasonably long.
fn usable(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_SESSION_ID && id.bytes().all(|b| b.is_ascii_graphic())
}

/// Why a session moved off its sticky deployment. Low cardinality by
/// construction: these are the only label values the reassignment counter
/// can carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Reassignment {
    /// The sticky deployment's node is unhealthy, or not yet known to be up.
    StickyUnhealthy,
    /// Its node is disabled, or no longer serves the deployment's model.
    StickyUnavailable,
    /// It is available but cannot serve this request (tools, reasoning,
    /// endpoint, context).
    StickyCapabilityMismatch,
    /// It was tried and refused the prompt as longer than its context; a
    /// larger deployment answered.
    StickyContextOverflow,
    /// It was tried and failed before answering — refused connection,
    /// timeout, 502/503/504, or a stale model.
    StickyFailed,
}

impl Reassignment {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StickyUnhealthy => "sticky_unhealthy",
            Self::StickyUnavailable => "sticky_unavailable",
            Self::StickyCapabilityMismatch => "sticky_capability_mismatch",
            Self::StickyContextOverflow => "sticky_context_overflow",
            Self::StickyFailed => "sticky_failed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::NodeId;
    use axum::http::HeaderValue;

    fn policy(ttl_secs: u64, max_entries: usize) -> AffinityPolicy {
        AffinityPolicy {
            enabled: true,
            header: "x-lightweight-session".into(),
            idle_ttl: Duration::from_secs(ttl_secs),
            max_entries,
        }
    }

    fn deployment(node: &str) -> DeploymentId {
        DeploymentId::of(&NodeId::parse(node).unwrap(), "M")
    }

    fn route(name: &str) -> RouteName {
        RouteName::parse(name).unwrap()
    }

    fn headers(session: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-lightweight-session",
            HeaderValue::from_str(session).unwrap(),
        );
        headers
    }

    #[test]
    fn no_header_or_affinity_off_means_no_session() {
        let book = AffinityBook::new(policy(60, 10));
        assert!(book.session(&route("Coder"), &HeaderMap::new()).is_none());
        assert!(book.session(&route("Coder"), &headers("abc")).is_some());
        assert!(book.session(&route("Coder"), &headers("   ")).is_none());
        let long = "x".repeat(MAX_SESSION_ID + 1);
        assert!(book.session(&route("Coder"), &headers(&long)).is_none());

        let off = AffinityBook::new(AffinityPolicy::default());
        assert!(off.session(&route("Coder"), &headers("abc")).is_none());
    }

    #[test]
    fn the_key_is_scoped_by_route_and_never_holds_the_raw_id() {
        let book = AffinityBook::new(policy(60, 10));
        let coder = book
            .session(&route("Coder"), &headers("session-123"))
            .unwrap();
        let research = book
            .session(&route("Research"), &headers("session-123"))
            .unwrap();
        assert_ne!(
            coder, research,
            "the same session on two routes is two keys"
        );
        assert!(!format!("{coder:?}").contains("session-123"));
        assert_eq!(coder.fingerprint().len(), 8);

        book.establish(coder.clone(), &deployment("a"));
        assert_eq!(book.lookup(&coder), Some(deployment("a")));
        assert_eq!(book.lookup(&research), None);
    }

    #[test]
    fn an_idle_entry_expires_and_a_used_one_does_not() {
        let book = AffinityBook::new(policy(10, 10));
        let key = book.key(&route("Coder"), "s");
        let start = Instant::now();
        book.establish_at(key.clone(), &deployment("a"), start);
        // Used at 8s: the idle clock restarts.
        let at = start + Duration::from_secs(8);
        assert_eq!(book.lookup_at(&key, at), Some(deployment("a")));
        let at = at + Duration::from_secs(8);
        assert_eq!(book.lookup_at(&key, at), Some(deployment("a")));
        // Idle for 11s: gone, and removed rather than left behind.
        let at = at + Duration::from_secs(11);
        assert_eq!(book.lookup_at(&key, at), None);
        assert!(book.is_empty());
        assert_eq!(book.evictions(), (1, 0));
    }

    #[test]
    fn the_sweep_removes_only_expired_entries() {
        let book = AffinityBook::new(policy(10, 10));
        let start = Instant::now();
        book.establish_at(book.key(&route("Coder"), "old"), &deployment("a"), start);
        let later = start + Duration::from_secs(9);
        book.establish_at(book.key(&route("Coder"), "new"), &deployment("b"), later);
        assert_eq!(book.sweep_at(start + Duration::from_secs(12)), 1);
        assert_eq!(book.len(), 1);
    }

    #[test]
    fn a_full_book_drops_expired_entries_first_then_the_least_recently_used() {
        let book = AffinityBook::new(policy(10, 2));
        let start = Instant::now();
        let k = |s: &str| book.key(&route("Coder"), s);
        book.establish_at(k("one"), &deployment("a"), start);
        book.establish_at(k("two"), &deployment("a"), start + Duration::from_secs(1));
        // Touch "one", so "two" is now the least recently used.
        book.lookup_at(&k("one"), start + Duration::from_secs(2));
        book.establish_at(k("three"), &deployment("b"), start + Duration::from_secs(3));
        assert_eq!(book.len(), 2, "never more than max_entries");
        assert!(
            book.lookup_at(&k("two"), start + Duration::from_secs(3))
                .is_none()
        );
        assert!(
            book.lookup_at(&k("one"), start + Duration::from_secs(3))
                .is_some()
        );
        assert_eq!(book.evictions().1, 1);

        // Twenty seconds on, both are expired: a new entry evicts by expiry,
        // not by capacity.
        book.establish_at(k("four"), &deployment("c"), start + Duration::from_secs(30));
        assert_eq!(book.len(), 1);
        assert_eq!(book.evictions().1, 1, "no capacity eviction was needed");
    }

    #[test]
    fn the_first_commit_establishes_and_a_reassignment_overrides() {
        let book = AffinityBook::new(policy(60, 10));
        let key = book.key(&route("Coder"), "s");
        assert_eq!(
            book.establish(key.clone(), &deployment("a")),
            Established::New
        );
        assert_eq!(
            book.establish(key.clone(), &deployment("a")),
            Established::Unchanged
        );
        // A racing first request that committed on b does not move it.
        assert_eq!(
            book.establish(key.clone(), &deployment("b")),
            Established::KeptExisting(deployment("a"))
        );
        assert_eq!(book.lookup(&key), Some(deployment("a")));
        // A reassignment — the sticky one could not serve — does.
        book.reassign(key.clone(), &deployment("b"));
        assert_eq!(book.lookup(&key), Some(deployment("b")));
    }

    #[test]
    fn concurrent_sessions_never_corrupt_the_book() {
        let book = std::sync::Arc::new(AffinityBook::new(policy(60, 100)));
        let threads: Vec<_> = (0..8)
            .map(|thread| {
                let book = std::sync::Arc::clone(&book);
                std::thread::spawn(move || {
                    for turn in 0..500 {
                        // Eight threads racing on the same ten sessions, each
                        // trying to establish its own deployment.
                        let key = book.key(&route("Coder"), &format!("s{}", turn % 10));
                        let mine = deployment(["a", "b"][thread % 2]);
                        match book.lookup(&key) {
                            Some(_) if turn % 7 == 0 => book.reassign(key, &mine),
                            Some(_) => {}
                            None => {
                                book.establish(key, &mine);
                            }
                        }
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(book.len(), 10, "one entry per session, no duplicates");
        for session in 0..10 {
            let key = book.key(&route("Coder"), &format!("s{session}"));
            let found = book.lookup(&key).unwrap();
            assert!(found == deployment("a") || found == deployment("b"));
        }
    }

    #[test]
    fn the_admin_view_shows_fingerprints_not_ids() {
        let book = AffinityBook::new(policy(60, 10));
        book.establish(
            book.key(&route("Coder"), "my-secret-session"),
            &deployment("a"),
        );
        let rows = book.entries();
        assert_eq!(rows.len(), 1);
        let text = serde_json::to_string(&rows).unwrap();
        assert!(!text.contains("my-secret-session"));
        assert!(text.contains("\"route\":\"Coder\""));
    }
}
