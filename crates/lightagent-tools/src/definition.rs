//! What a tool is: its declaration and its behaviour.

use async_trait::async_trait;
use lightagent_core::{RiskClass, Scope, ToolOutcome, ToolSchema};
use serde_json::Value;

use crate::context::ToolCtx;

/// A tool's stable declaration.
///
/// The `parameters` are a JSON Schema declared to the model *and* enforced
/// locally by [`schema::validate`](crate::schema::validate) before the tool
/// runs, so the two can never drift. `risk` and `scopes` are what the policy
/// reads to decide whether a call may run unattended.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolDefinition {
    /// The namespaced name the model calls, e.g. `datetime.now`.
    pub name: String,
    /// A one-line description declared to the model.
    pub description: String,
    /// A JSON Schema for the arguments; an empty object means "no arguments".
    pub parameters: Value,
    /// The risk class the policy classifies this tool by.
    pub risk: RiskClass,
    /// The capabilities this tool requires, for scope-aware grants.
    pub scopes: Vec<Scope>,
}

impl ToolDefinition {
    /// A definition with the given fields.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        risk: RiskClass,
        scopes: Vec<Scope>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            risk,
            scopes,
        }
    }

    /// The shape declared to the model — name, description and the schema.
    pub fn to_schema(&self) -> ToolSchema {
        ToolSchema::new(
            self.name.clone(),
            self.description.clone(),
            self.parameters.clone(),
        )
    }
}

/// A callable tool.
///
/// `call` receives arguments the [`BoundedExecutor`](crate::BoundedExecutor) has
/// already parsed and schema-validated, so an implementation reads the fields it
/// declared without re-checking their shape. A failure is returned as a
/// [`ToolOutcome::error`] the model is shown, not raised — the run continues.
#[async_trait]
pub trait Tool: Send + Sync {
    /// This tool's declaration.
    fn definition(&self) -> &ToolDefinition;

    /// Run the tool against already-validated `args`.
    async fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolOutcome;
}
