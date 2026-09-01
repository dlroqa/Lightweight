//! Permission vocabulary.
//!
//! Slice 1 defines only the words: the risk classes a tool can carry and the
//! scopes it can require. Policy evaluation, approval records and the
//! `AwaitingApproval` state machine are Slice 4 — but the vocabulary lands here
//! now so a tool declared in Slice 3 can already state its risk, and the loop
//! can carry it, without a later type change rippling through.

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
}
