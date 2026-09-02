//! `terminal.run` — run one bounded program in the workspace.
//!
//! [`RiskClass::Executable`](lightagent_core::RiskClass::Executable), so the
//! default policy pauses for approval before anything runs. It executes a single
//! program directly — never through a shell — so there is no shell-injection
//! surface: the model supplies a program and an argument list, not a command
//! line. The working directory is confined to the workspace, the run is bounded
//! by a timeout (the child is killed if it overruns), and its output is capped.
//!
//! Confinement note: the working directory is inside the workspace, but a program
//! that runs can still reach whatever the user can — execution is a real grant,
//! which is why it is approval-gated and separately enabled (`allow_terminal`).

use std::process::Stdio;

use async_trait::async_trait;
use lightagent_core::{RiskClass, Scope, ToolOutcome};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::context::ToolCtx;
use crate::definition::{Tool, ToolDefinition};
use crate::output::clamp;

/// `terminal.run` — run a bounded program.
pub struct TerminalRun {
    definition: ToolDefinition,
}

impl TerminalRun {
    pub const NAME: &'static str = "terminal.run";

    pub fn new() -> Self {
        let parameters = json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The program to run (no shell)." },
                "args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Arguments passed to the program.",
                },
                "cwd": {
                    "type": "string",
                    "description": "Workspace-relative working directory; the root when omitted.",
                }
            },
            "required": ["command"],
            "additionalProperties": false,
        });
        Self {
            definition: ToolDefinition::new(
                Self::NAME,
                "Run a single program (no shell) in the workspace and return its output.",
                parameters,
                RiskClass::Executable,
                vec![Scope::new("terminal:exec")],
            ),
        }
    }
}

impl Default for TerminalRun {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct RunArgs {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    cwd: Option<String>,
}

#[async_trait]
impl Tool for TerminalRun {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolOutcome {
        let Some(ws) = ctx.workspace.clone() else {
            return ToolOutcome::error("terminal access is not enabled for this run");
        };
        if !ws.policy.allow_terminal {
            return ToolOutcome::error("terminal.run is disabled (set tools.allow_terminal)");
        }
        let Ok(args) = serde_json::from_value::<RunArgs>(args.clone()) else {
            return ToolOutcome::error("could not read terminal.run arguments");
        };
        if !ws.policy.terminal_allowlist.is_empty()
            && !ws
                .policy
                .terminal_allowlist
                .iter()
                .any(|p| p == &args.command)
        {
            return ToolOutcome::error(format!(
                "program {:?} is not in tools.terminal_allowlist",
                args.command
            ));
        }
        let cwd = match &args.cwd {
            Some(rel) => match ws.workspace.resolve_existing(rel) {
                Ok(dir) => dir,
                Err(message) => return ToolOutcome::error(message),
            },
            None => ws.workspace.root().to_path_buf(),
        };

        let mut command = tokio::process::Command::new(&args.command);
        command
            .args(&args.args)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return ToolOutcome::error(format!("could not start {:?}: {error}", args.command));
            }
        };
        // On timeout the future is dropped, and `kill_on_drop` kills the child.
        let output = match tokio::time::timeout(
            ws.policy.terminal_timeout,
            child.wait_with_output(),
        )
        .await
        {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => {
                return ToolOutcome::error(format!("{:?} failed: {error}", args.command));
            }
            Err(_) => {
                return ToolOutcome::error(format!(
                    "{:?} timed out after {:?}",
                    args.command, ws.policy.terminal_timeout
                ));
            }
        };

        let cap = ws.policy.max_file_bytes;
        let stdout = clamp(String::from_utf8_lossy(&output.stdout).into_owned(), cap);
        let stderr = clamp(String::from_utf8_lossy(&output.stderr).into_owned(), cap);
        let code = output
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".to_owned());

        let mut rendered = format!("exit: {code}\n");
        if !stdout.is_empty() {
            rendered.push_str(&format!("stdout:\n{stdout}\n"));
        }
        if !stderr.is_empty() {
            rendered.push_str(&format!("stderr:\n{stderr}\n"));
        }
        ToolOutcome {
            content: rendered.trim_end().to_owned(),
            is_error: !output.status.success(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{WorkspaceContext, WorkspacePolicy};
    use crate::workspace::Workspace;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    fn scratch() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lightagent-term-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ctx_for(root: &std::path::Path, allow: bool, allowlist: Vec<String>) -> ToolCtx {
        let policy = WorkspacePolicy {
            max_file_bytes: 1_000_000,
            allow_terminal: allow,
            terminal_timeout: Duration::from_secs(10),
            terminal_allowlist: allowlist,
        };
        ToolCtx::new(CancellationToken::new()).with_workspace(WorkspaceContext {
            workspace: Arc::new(Workspace::new(root).unwrap()),
            policy: Arc::new(policy),
        })
    }

    #[tokio::test]
    async fn runs_a_program_and_captures_output() {
        let root = scratch();
        let ctx = ctx_for(&root, true, Vec::new());
        let out = TerminalRun::new()
            .call(&json!({ "command": "echo", "args": ["hi"] }), &ctx)
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("hi"));
        assert!(out.content.contains("exit: 0"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn disabled_terminal_is_refused() {
        let root = scratch();
        let ctx = ctx_for(&root, false, Vec::new());
        let out = TerminalRun::new()
            .call(&json!({ "command": "echo" }), &ctx)
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("disabled"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn an_allowlist_blocks_other_programs() {
        let root = scratch();
        let ctx = ctx_for(&root, true, vec!["echo".to_owned()]);
        let out = TerminalRun::new()
            .call(&json!({ "command": "ls" }), &ctx)
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("allowlist"));
        std::fs::remove_dir_all(&root).ok();
    }
}
