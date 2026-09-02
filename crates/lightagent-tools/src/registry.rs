//! The set of tools a run may use.
//!
//! A registry maps a name to a [`Tool`]. [`builtin`](ToolRegistry::builtin)
//! assembles the standard set; [`scoped`](ToolRegistry::scoped) narrows a
//! registry to an allow-list, which is how a worker run is handed a subset of
//! its orchestrator's tools; [`schemas`](ToolRegistry::schemas) is what the loop
//! declares to the model.

use std::collections::BTreeMap;
use std::sync::Arc;

use lightagent_core::ToolSchema;

use crate::builtins::{AgentDelegate, DateTimeNow};
use crate::definition::Tool;

/// A named, immutable-once-built set of tools.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a tool, replacing any existing one with the same name.
    pub fn with(mut self, tool: Arc<dyn Tool>) -> Self {
        self.insert(tool);
        self
    }

    /// Insert a tool, replacing any existing one with the same name.
    pub fn insert(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.definition().name.clone();
        self.tools.insert(name, tool);
    }

    /// The standard built-in set: `datetime.now` and `agent.delegate`.
    pub fn builtin() -> Self {
        Self::new()
            .with(Arc::new(DateTimeNow::new()))
            .with(Arc::new(AgentDelegate::new()))
    }

    /// The built-in set a worker is given: everything except `agent.delegate`,
    /// so delegation is one level deep.
    pub fn worker_default() -> Self {
        Self::builtin().without(AgentDelegate::NAME)
    }

    /// This registry without the named tool.
    pub fn without(mut self, name: &str) -> Self {
        self.tools.remove(name);
        self
    }

    /// A registry narrowed to `allow`, preserving only names present in both.
    /// An empty allow-list yields an empty registry (a worker offered nothing).
    pub fn scoped(&self, allow: &[String]) -> Self {
        let mut tools = BTreeMap::new();
        for name in allow {
            if let Some(tool) = self.tools.get(name) {
                tools.insert(name.clone(), Arc::clone(tool));
            }
        }
        Self { tools }
    }

    /// Look up a tool by name.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    /// Whether a tool with this name is present.
    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// The tool names, sorted.
    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// The schemas declared to the model, one per tool.
    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools
            .values()
            .map(|tool| tool.definition().to_schema())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_lists_datetime_and_delegate() {
        let registry = ToolRegistry::builtin();
        assert!(registry.contains("datetime.now"));
        assert!(registry.contains("agent.delegate"));
        assert_eq!(registry.schemas().len(), 2);
    }

    #[test]
    fn worker_default_excludes_delegate() {
        let registry = ToolRegistry::worker_default();
        assert!(registry.contains("datetime.now"));
        assert!(!registry.contains("agent.delegate"));
    }

    #[test]
    fn scoped_keeps_only_the_allow_list() {
        let registry = ToolRegistry::builtin().scoped(&["datetime.now".into()]);
        assert_eq!(registry.names(), vec!["datetime.now".to_string()]);
    }

    #[test]
    fn scoped_ignores_unknown_names() {
        let registry = ToolRegistry::builtin().scoped(&["nope".into()]);
        assert!(registry.names().is_empty());
    }
}
