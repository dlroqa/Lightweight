//! `skill.read` — load a skill's full instructions by name.
//!
//! The read side of progressive disclosure: the model sees each skill's name and
//! description in the system prompt (the catalog) and calls this to pull one
//! skill's body only when it is relevant. [`RiskClass::Observe`](lightagent_core::
//! RiskClass::Observe) — it reads local, declared instructions and changes
//! nothing — so the default policy runs it unattended. It works only when the
//! caller injected a [`SkillContext`]; absent it, it returns a controlled error.

use async_trait::async_trait;
use lightagent_core::{RiskClass, Scope, ToolOutcome};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::context::ToolCtx;
use crate::definition::{Tool, ToolDefinition};

/// The `skill.read` tool.
pub struct SkillRead {
    definition: ToolDefinition,
}

impl SkillRead {
    pub const NAME: &'static str = "skill.read";

    pub fn new() -> Self {
        let parameters = json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "The name of the skill to load." }
            },
            "required": ["name"],
            "additionalProperties": false,
        });
        Self {
            definition: ToolDefinition::new(
                Self::NAME,
                "Load a skill's full instructions by name.",
                parameters,
                RiskClass::Observe,
                vec![Scope::new("skill:read")],
            ),
        }
    }
}

impl Default for SkillRead {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct ReadArgs {
    name: String,
}

#[async_trait]
impl Tool for SkillRead {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolOutcome {
        let Some(context) = ctx.skills.as_ref() else {
            return ToolOutcome::error("no skills are available for this run");
        };
        let Ok(args) = serde_json::from_value::<ReadArgs>(args.clone()) else {
            return ToolOutcome::error("could not read skill.read arguments");
        };
        match context.skills.get(&args.name) {
            Some(skill) => ToolOutcome::ok(skill.body.clone()),
            None => ToolOutcome::error(format!("no skill named {:?}", args.name)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::SkillContext;
    use lightagent_core::SkillStore;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    fn store_with_one() -> SkillStore {
        let dir = std::env::temp_dir().join(format!("lightagent-skillread-{}", std::process::id()));
        let skill = dir.join("greet");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: greet\ndescription: Say hi\n---\nAlways greet warmly.",
        )
        .unwrap();
        let store = SkillStore::load(std::slice::from_ref(&dir));
        std::fs::remove_dir_all(&dir).ok();
        store
    }

    #[tokio::test]
    async fn reads_a_known_skill() {
        let ctx = ToolCtx::new(CancellationToken::new()).with_skills(SkillContext {
            skills: Arc::new(store_with_one()),
        });
        let out = SkillRead::new()
            .call(&json!({ "name": "greet" }), &ctx)
            .await;
        assert!(!out.is_error);
        assert_eq!(out.content, "Always greet warmly.");

        let missing = SkillRead::new()
            .call(&json!({ "name": "nope" }), &ctx)
            .await;
        assert!(missing.is_error);
    }

    #[tokio::test]
    async fn without_context_is_a_controlled_error() {
        let ctx = ToolCtx::new(CancellationToken::new());
        let out = SkillRead::new()
            .call(&json!({ "name": "greet" }), &ctx)
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("no skills"));
    }
}
