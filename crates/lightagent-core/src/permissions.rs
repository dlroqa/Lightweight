//! Permission vocabulary and approval policy.
//!
//! Slice 1 defined the words: the risk classes a tool can carry and the scopes
//! it can require. Slice 4 grows that into a working policy: an
//! [`ApprovalPolicy`] that a [`PolicyEngine`] evaluates against an
//! [`ApprovalRequest`] to an [`ApprovalNeed`] (auto-approve, require a human
//! decision, or deny outright), the [`ApprovalDecision`] a human returns, the
//! [`ApprovalRecord`] a remembered grant becomes, and an [`ApprovalStore`] that
//! persists those records per profile.
//!
//! The struct [`ApprovalPolicy`] here is the *fine-grained* policy the engine
//! reasons over (a risk ceiling, a deny list, and remembered grants). It is
//! derived from the coarse user setting [`crate::config::ApprovalPolicy`] via
//! `From`, so the profile on disk keeps its simple `permissive`/`balanced`/
//! `strict` word and the engine turns it into concrete rules.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// How dangerous a tool is, from least to most.
///
/// Ordered so a policy can say "approve anything at or above `Mutating`" as a
/// simple comparison. The ordering is the point of the `PartialOrd`/`Ord`
/// derive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskClass {
    /// Reads state without changing anything (`datetime.now`, a file read).
    Observe,
    /// Reaches the network for data, but changes nothing local.
    External,
    /// Touches private or credential-bearing data.
    Sensitive,
    /// Changes local state (a file write).
    Mutating,
    /// Runs code or a subprocess.
    Executable,
    /// Requires elevated privilege.
    Privileged,
}

impl RiskClass {
    /// The stable wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Observe => "observe",
            Self::External => "external",
            Self::Sensitive => "sensitive",
            Self::Mutating => "mutating",
            Self::Executable => "executable",
            Self::Privileged => "privileged",
        }
    }
}

/// A named capability a tool requires, e.g. `fs:read` or `net:fetch`.
///
/// A free-form namespaced string this slice: the grammar it belongs to is
/// Slice 4's, and forcing an enum now would guess at categories that policy
/// evaluation has not yet defined.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Scope(String);

impl Scope {
    pub fn new(scope: impl Into<String>) -> Self {
        Self(scope.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A unique id for one approval request, safe in a log line, a filename or a URL.
///
/// Minted like a [`RunId`](crate::ids::RunId): 128 bits of OS entropy when
/// available, a monotonic-counter-plus-clock fallback when it is not, so a
/// request can always be raised.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ApprovalId(String);

impl ApprovalId {
    /// Mint a new approval id. Infallible by construction.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let mut bytes = [0u8; 16];
        if getrandom::fill(&mut bytes).is_ok() {
            return Self(format!("apr-{}", hex(&bytes)));
        }
        Self(format!("apr-{}", fallback()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ApprovalId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Render bytes as lowercase hex, no separators.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(nibble(byte >> 4));
        out.push(nibble(byte & 0x0f));
    }
    out
}

fn nibble(value: u8) -> char {
    match value {
        0..=9 => (b'0' + value) as char,
        _ => (b'a' + (value - 10)) as char,
    }
}

/// A collision-resistant identifier without OS entropy.
fn fallback() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0);
    format!("{nanos:016x}{sequence:016x}")
}

/// The fine-grained policy a [`PolicyEngine`] reasons over.
///
/// A call is auto-approved when its risk is at or below `auto_approve_max` (or a
/// remembered grant covers it), denied when its risk is in `deny`, and requires
/// a human decision otherwise. `deny` wins over everything.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalPolicy {
    /// The highest risk that runs without asking.
    pub auto_approve_max: RiskClass,
    /// Remembered grants that stand in for a fresh decision while unexpired.
    #[serde(default)]
    pub grants: Vec<ApprovalRecord>,
    /// Risk classes that are refused outright, before any grant is consulted.
    #[serde(default)]
    pub deny: Vec<RiskClass>,
}

impl Default for ApprovalPolicy {
    /// The `balanced` default: read and fetch run freely, anything that touches
    /// private data or beyond needs a decision, and privilege is refused.
    fn default() -> Self {
        Self::balanced()
    }
}

impl ApprovalPolicy {
    /// Approve everything; deny nothing. For trusted, unattended runs.
    pub fn permissive() -> Self {
        Self {
            auto_approve_max: RiskClass::Privileged,
            grants: Vec::new(),
            deny: Vec::new(),
        }
    }

    /// The default posture: auto-approve up to [`RiskClass::External`], deny
    /// [`RiskClass::Privileged`].
    pub fn balanced() -> Self {
        Self {
            auto_approve_max: RiskClass::External,
            grants: Vec::new(),
            deny: vec![RiskClass::Privileged],
        }
    }

    /// Prompt for anything above a bare read; deny privilege.
    pub fn strict() -> Self {
        Self {
            auto_approve_max: RiskClass::Observe,
            grants: Vec::new(),
            deny: vec![RiskClass::Privileged],
        }
    }
}

impl From<crate::config::ApprovalPolicy> for ApprovalPolicy {
    fn from(coarse: crate::config::ApprovalPolicy) -> Self {
        match coarse {
            crate::config::ApprovalPolicy::Permissive => Self::permissive(),
            crate::config::ApprovalPolicy::Balanced => Self::balanced(),
            crate::config::ApprovalPolicy::Strict => Self::strict(),
        }
    }
}

/// A request for a decision on one tool call.
///
/// The `arguments_preview` is a *safe* rendering: it is bounded in size and has
/// secret-looking values redacted, so raising a request never leaks a key into
/// a prompt, a log or a UI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub id: ApprovalId,
    pub tool: String,
    pub risk: RiskClass,
    #[serde(default)]
    pub scopes: Vec<Scope>,
    #[serde(default)]
    pub arguments_preview: String,
}

impl ApprovalRequest {
    /// A request with a freshly minted id.
    pub fn new(
        tool: impl Into<String>,
        risk: RiskClass,
        scopes: Vec<Scope>,
        arguments_preview: impl Into<String>,
    ) -> Self {
        Self {
            id: ApprovalId::new(),
            tool: tool.into(),
            risk,
            scopes,
            arguments_preview: arguments_preview.into(),
        }
    }
}

/// What a policy decided about a call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApprovalNeed {
    /// The call may run now.
    AutoApprove,
    /// The call needs a human decision; here is the request to raise.
    Require(ApprovalRequest),
    /// The call is refused; the string says why.
    Deny(String),
}

/// A human's answer to an [`ApprovalRequest`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalDecision {
    pub id: ApprovalId,
    pub granted: bool,
    /// When set, the grant is remembered for this long, so an identical call
    /// does not ask again until it expires.
    pub remember: Option<Duration>,
}

impl ApprovalDecision {
    /// Grant this once, without remembering it.
    pub fn grant(id: ApprovalId) -> Self {
        Self {
            id,
            granted: true,
            remember: None,
        }
    }

    /// Grant, and remember the grant for `ttl`.
    pub fn grant_for(id: ApprovalId, ttl: Duration) -> Self {
        Self {
            id,
            granted: true,
            remember: Some(ttl),
        }
    }

    /// Refuse this call.
    pub fn deny(id: ApprovalId) -> Self {
        Self {
            id,
            granted: false,
            remember: None,
        }
    }
}

/// A remembered grant: a decision that stands in for a fresh one until it
/// expires.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRecord {
    pub tool: String,
    pub risk: RiskClass,
    #[serde(default)]
    pub scopes: Vec<Scope>,
    pub granted_at: SystemTime,
    /// `None` never expires; `Some(t)` is valid strictly before `t`.
    #[serde(default)]
    pub expires_at: Option<SystemTime>,
}

impl ApprovalRecord {
    /// Build a record for `request`, remembered for `ttl` from `now` (or
    /// forever when `ttl` is `None`).
    pub fn from_request(request: &ApprovalRequest, now: SystemTime, ttl: Option<Duration>) -> Self {
        Self {
            tool: request.tool.clone(),
            risk: request.risk,
            scopes: request.scopes.clone(),
            granted_at: now,
            expires_at: ttl.map(|ttl| now + ttl),
        }
    }

    /// Whether this record covers `request` at `now`: the same tool at the same
    /// risk, covering every scope the request needs, and not yet expired.
    pub fn covers(&self, request: &ApprovalRequest, now: SystemTime) -> bool {
        if self.tool != request.tool || self.risk != request.risk {
            return false;
        }
        if !request
            .scopes
            .iter()
            .all(|scope| self.scopes.contains(scope))
        {
            return false;
        }
        match self.expires_at {
            Some(expiry) => now < expiry,
            None => true,
        }
    }
}

/// Evaluates a policy against a request.
///
/// Holds the policy (including any remembered grants) and answers, for a given
/// `now`, whether a call auto-approves, needs a decision, or is denied.
#[derive(Clone, Debug)]
pub struct PolicyEngine {
    policy: ApprovalPolicy,
}

impl PolicyEngine {
    pub fn new(policy: ApprovalPolicy) -> Self {
        Self { policy }
    }

    /// The policy this engine enforces.
    pub fn policy(&self) -> &ApprovalPolicy {
        &self.policy
    }

    /// Remember a grant, so a matching request auto-approves until it expires.
    pub fn remember(&mut self, record: ApprovalRecord) {
        self.policy.grants.push(record);
    }

    /// Decide `request` as of `now`.
    ///
    /// The order is deliberate: a denied risk is refused before any grant is
    /// consulted, so a remembered grant can never resurrect a class the policy
    /// forbids.
    pub fn evaluate(&self, request: &ApprovalRequest, now: SystemTime) -> ApprovalNeed {
        if self.policy.deny.contains(&request.risk) {
            return ApprovalNeed::Deny(format!(
                "the tool '{}' is denied by policy (risk class '{}')",
                request.tool,
                request.risk.as_str()
            ));
        }
        if self
            .policy
            .grants
            .iter()
            .any(|grant| grant.covers(request, now))
        {
            return ApprovalNeed::AutoApprove;
        }
        if request.risk <= self.policy.auto_approve_max {
            return ApprovalNeed::AutoApprove;
        }
        ApprovalNeed::Require(request.clone())
    }
}

/// A per-profile append-only log of remembered grants (`approvals.jsonl`).
///
/// One JSON [`ApprovalRecord`] per line. Reads tolerate blank lines; a line
/// that will not parse is an error rather than a silent skip, so a corrupt log
/// is noticed. Writes go through the owner-only atomic path.
#[derive(Clone, Debug)]
pub struct ApprovalStore {
    path: PathBuf,
}

impl ApprovalStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every record in the log, in file order.
    pub fn list(&self) -> Result<Vec<ApprovalRecord>, ApprovalStoreError> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(ApprovalStoreError::Io(err.to_string())),
        };
        let mut records = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let record = serde_json::from_str(line)
                .map_err(|err| ApprovalStoreError::Corrupt(err.to_string()))?;
            records.push(record);
        }
        Ok(records)
    }

    /// Append one record.
    pub fn append(&self, record: &ApprovalRecord) -> Result<(), ApprovalStoreError> {
        let mut records = self.list()?;
        records.push(record.clone());
        self.rewrite(&records)
    }

    /// Drop expired records, keeping the log small. Returns how many remain.
    pub fn prune_expired(&self, now: SystemTime) -> Result<usize, ApprovalStoreError> {
        let kept: Vec<ApprovalRecord> = self
            .list()?
            .into_iter()
            .filter(|record| match record.expires_at {
                Some(expiry) => now < expiry,
                None => true,
            })
            .collect();
        let remaining = kept.len();
        self.rewrite(&kept)?;
        Ok(remaining)
    }

    fn rewrite(&self, records: &[ApprovalRecord]) -> Result<(), ApprovalStoreError> {
        let mut bytes = Vec::new();
        for record in records {
            let line = serde_json::to_vec(record)
                .map_err(|err| ApprovalStoreError::Corrupt(err.to_string()))?;
            bytes.extend_from_slice(&line);
            bytes.push(b'\n');
        }
        crate::paths::write_private(&self.path, &bytes)
            .map_err(|err| ApprovalStoreError::Io(err.to_string()))
    }
}

/// Why an approval-store operation failed.
#[derive(Debug, thiserror::Error)]
pub enum ApprovalStoreError {
    #[error("the approvals log is corrupt: {0}")]
    Corrupt(String),
    #[error("an approvals-log operation failed: {0}")]
    Io(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn risk_classes_are_ordered_least_to_most_dangerous() {
        assert!(RiskClass::Observe < RiskClass::Mutating);
        assert!(RiskClass::Mutating < RiskClass::Privileged);
        assert!(RiskClass::External < RiskClass::Executable);
    }

    #[test]
    fn risk_class_round_trips_through_json() {
        let json = serde_json::to_string(&RiskClass::Executable).expect("serialize");
        assert_eq!(json, "\"executable\"");
        let back: RiskClass = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, RiskClass::Executable);
    }

    fn request(tool: &str, risk: RiskClass, scopes: Vec<Scope>) -> ApprovalRequest {
        ApprovalRequest::new(tool, risk, scopes, "{}")
    }

    #[test]
    fn risk_ordering_and_auto_approve() {
        // Balanced auto-approves up to External and requires above it.
        let engine = PolicyEngine::new(ApprovalPolicy::balanced());
        let now = SystemTime::now();
        assert_eq!(
            engine.evaluate(&request("datetime.now", RiskClass::Observe, vec![]), now),
            ApprovalNeed::AutoApprove
        );
        assert_eq!(
            engine.evaluate(&request("web.fetch", RiskClass::External, vec![]), now),
            ApprovalNeed::AutoApprove
        );
        assert!(matches!(
            engine.evaluate(&request("fs.write", RiskClass::Mutating, vec![]), now),
            ApprovalNeed::Require(_)
        ));
    }

    #[test]
    fn privileged_is_denied() {
        let engine = PolicyEngine::new(ApprovalPolicy::balanced());
        let need = engine.evaluate(
            &request("sudo.run", RiskClass::Privileged, vec![]),
            SystemTime::now(),
        );
        assert!(matches!(need, ApprovalNeed::Deny(_)));
    }

    #[test]
    fn record_expiry() {
        let req = request("fs.write", RiskClass::Mutating, vec![]);
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let record = ApprovalRecord::from_request(&req, base, Some(Duration::from_secs(60)));
        // Covers before expiry, not at or after it.
        assert!(record.covers(&req, base + Duration::from_secs(59)));
        assert!(!record.covers(&req, base + Duration::from_secs(60)));
        assert!(!record.covers(&req, base + Duration::from_secs(61)));
    }

    #[test]
    fn remembered_grant_covers_next_call() {
        let mut engine = PolicyEngine::new(ApprovalPolicy::strict());
        let req = request("fs.write", RiskClass::Mutating, vec![]);
        let now = SystemTime::now();
        // Strict requires this call...
        assert!(matches!(
            engine.evaluate(&req, now),
            ApprovalNeed::Require(_)
        ));
        // ...until a matching grant is remembered.
        engine.remember(ApprovalRecord::from_request(
            &req,
            now,
            Some(Duration::from_secs(300)),
        ));
        assert_eq!(engine.evaluate(&req, now), ApprovalNeed::AutoApprove);
    }

    #[test]
    fn scope_mismatch_still_requires() {
        let mut engine = PolicyEngine::new(ApprovalPolicy::strict());
        let granted = request("fs.write", RiskClass::Mutating, vec![Scope::new("fs:read")]);
        let now = SystemTime::now();
        engine.remember(ApprovalRecord::from_request(&granted, now, None));

        // A call needing a scope the grant does not cover is not auto-approved.
        let wider = request(
            "fs.write",
            RiskClass::Mutating,
            vec![Scope::new("fs:read"), Scope::new("fs:write")],
        );
        assert!(matches!(
            engine.evaluate(&wider, now),
            ApprovalNeed::Require(_)
        ));
    }

    #[test]
    fn approval_store_round_trips_and_prunes() {
        let dir = std::env::temp_dir().join(format!(
            "lightagent-approvals-{}",
            crate::ids::RunId::new().as_str()
        ));
        let store = ApprovalStore::new(dir.join("approvals.jsonl"));
        assert!(store.list().expect("empty list").is_empty());

        let now = SystemTime::now();
        let live = ApprovalRecord::from_request(
            &request("fs.write", RiskClass::Mutating, vec![]),
            now,
            Some(Duration::from_secs(300)),
        );
        let expired = ApprovalRecord::from_request(
            &request("fs.read", RiskClass::Observe, vec![]),
            now - Duration::from_secs(600),
            Some(Duration::from_secs(60)),
        );
        store.append(&live).expect("append live");
        store.append(&expired).expect("append expired");
        assert_eq!(store.list().expect("list").len(), 2);

        let remaining = store.prune_expired(now).expect("prune");
        assert_eq!(remaining, 1);
        let kept = store.list().expect("list after prune");
        assert_eq!(kept, vec![live]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn coarse_policy_maps_to_rules() {
        assert_eq!(
            ApprovalPolicy::from(crate::config::ApprovalPolicy::Balanced),
            ApprovalPolicy::balanced()
        );
        assert_eq!(
            ApprovalPolicy::from(crate::config::ApprovalPolicy::Strict).auto_approve_max,
            RiskClass::Observe
        );
        assert!(
            ApprovalPolicy::from(crate::config::ApprovalPolicy::Permissive)
                .deny
                .is_empty()
        );
    }
}
