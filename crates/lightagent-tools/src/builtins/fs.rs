//! `fs.read`, `fs.list` and `fs.write` — files within the confined workspace.
//!
//! Every path is resolved through the run's [`Workspace`](crate::workspace::
//! Workspace), so a tool can only ever touch the workspace tree; `..`, absolute
//! paths and symlinks that leave it are refused. The tools run only when the
//! caller injected a [`WorkspaceContext`]; absent it they return a controlled
//! result. Reads and writes are capped at `max_file_bytes`.
//!
//! `fs.read`/`fs.list` are [`RiskClass::Observe`](lightagent_core::RiskClass::
//! Observe); `fs.write` is [`RiskClass::Mutating`](lightagent_core::RiskClass::
//! Mutating), so the default policy pauses for approval before it changes a file.

use async_trait::async_trait;
use lightagent_core::{RiskClass, Scope, ToolOutcome};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt as _;

use crate::context::{ToolCtx, WorkspaceContext};
use crate::definition::{Tool, ToolDefinition};

fn workspace(ctx: &ToolCtx) -> Result<WorkspaceContext, ToolOutcome> {
    ctx.workspace
        .clone()
        .ok_or_else(|| ToolOutcome::error("filesystem access is not enabled for this run"))
}

/// `fs.read` — read a text file from the workspace.
pub struct FsRead {
    definition: ToolDefinition,
}

impl FsRead {
    pub const NAME: &'static str = "fs.read";

    pub fn new() -> Self {
        let parameters = json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Workspace-relative path to read." }
            },
            "required": ["path"],
            "additionalProperties": false,
        });
        Self {
            definition: ToolDefinition::new(
                Self::NAME,
                "Read a text file from the workspace.",
                parameters,
                RiskClass::Observe,
                vec![Scope::new("fs:read")],
            ),
        }
    }
}

impl Default for FsRead {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct PathArg {
    path: String,
}

#[async_trait]
impl Tool for FsRead {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolOutcome {
        let ws = match workspace(ctx) {
            Ok(ws) => ws,
            Err(outcome) => return outcome,
        };
        let Ok(args) = serde_json::from_value::<PathArg>(args.clone()) else {
            return ToolOutcome::error("could not read fs.read arguments");
        };
        let path = match ws.workspace.resolve_existing(&args.path) {
            Ok(path) => path,
            Err(message) => return ToolOutcome::error(message),
        };
        let cap = ws.policy.max_file_bytes;
        let file = match tokio::fs::File::open(&path).await {
            Ok(file) => file,
            Err(error) => {
                return ToolOutcome::error(format!("could not open {:?}: {error}", args.path));
            }
        };
        let mut buffer = Vec::new();
        if let Err(error) = file.take(cap as u64 + 1).read_to_end(&mut buffer).await {
            return ToolOutcome::error(format!("could not read {:?}: {error}", args.path));
        }
        let truncated = buffer.len() > cap;
        buffer.truncate(cap);
        let mut text = String::from_utf8_lossy(&buffer).into_owned();
        if truncated {
            text.push_str("\n…[truncated at max_file_bytes]");
        }
        ToolOutcome::ok(text)
    }
}

/// `fs.list` — list a directory in the workspace.
pub struct FsList {
    definition: ToolDefinition,
}

impl FsList {
    pub const NAME: &'static str = "fs.list";

    pub fn new() -> Self {
        let parameters = json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Workspace-relative directory to list; the root when omitted.",
                }
            },
            "additionalProperties": false,
        });
        Self {
            definition: ToolDefinition::new(
                Self::NAME,
                "List the entries of a directory in the workspace.",
                parameters,
                RiskClass::Observe,
                vec![Scope::new("fs:read")],
            ),
        }
    }
}

impl Default for FsList {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct ListArgs {
    #[serde(default)]
    path: Option<String>,
}

#[async_trait]
impl Tool for FsList {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolOutcome {
        let ws = match workspace(ctx) {
            Ok(ws) => ws,
            Err(outcome) => return outcome,
        };
        let Ok(args) = serde_json::from_value::<ListArgs>(args.clone()) else {
            return ToolOutcome::error("could not read fs.list arguments");
        };
        let relative = args.path.unwrap_or_else(|| ".".to_owned());
        let dir = match ws.workspace.resolve_existing(&relative) {
            Ok(dir) => dir,
            Err(message) => return ToolOutcome::error(message),
        };
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(entries) => entries,
            Err(error) => {
                return ToolOutcome::error(format!("could not list {relative:?}: {error}"));
            }
        };
        let mut rows = Vec::new();
        loop {
            match entries.next_entry().await {
                Ok(Some(entry)) => {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let (kind, size) = match entry.metadata().await {
                        Ok(meta) if meta.is_dir() => ("dir", 0),
                        Ok(meta) => ("file", meta.len()),
                        Err(_) => ("?", 0),
                    };
                    rows.push((name, kind, size));
                }
                Ok(None) => break,
                Err(error) => {
                    return ToolOutcome::error(format!("could not list {relative:?}: {error}"));
                }
            }
        }
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        if rows.is_empty() {
            return ToolOutcome::ok(format!("{relative} is empty."));
        }
        let mut out = format!("{relative}:\n");
        for (name, kind, size) in rows {
            if kind == "dir" {
                out.push_str(&format!("  {name}/\n"));
            } else {
                out.push_str(&format!("  {name} ({size} bytes)\n"));
            }
        }
        ToolOutcome::ok(out.trim_end().to_owned())
    }
}

/// `fs.write` — write (or append to) a file in the workspace.
pub struct FsWrite {
    definition: ToolDefinition,
}

impl FsWrite {
    pub const NAME: &'static str = "fs.write";

    pub fn new() -> Self {
        let parameters = json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Workspace-relative path to write." },
                "content": { "type": "string", "description": "The text to write." },
                "append": {
                    "type": "boolean",
                    "description": "Append instead of overwriting; default false.",
                }
            },
            "required": ["path", "content"],
            "additionalProperties": false,
        });
        Self {
            definition: ToolDefinition::new(
                Self::NAME,
                "Write or append text to a file in the workspace.",
                parameters,
                RiskClass::Mutating,
                vec![Scope::new("fs:write")],
            ),
        }
    }
}

impl Default for FsWrite {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
    #[serde(default)]
    append: bool,
}

#[async_trait]
impl Tool for FsWrite {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolOutcome {
        let ws = match workspace(ctx) {
            Ok(ws) => ws,
            Err(outcome) => return outcome,
        };
        let Ok(args) = serde_json::from_value::<WriteArgs>(args.clone()) else {
            return ToolOutcome::error("could not read fs.write arguments");
        };
        let bytes = args.content.as_bytes();
        if bytes.len() > ws.policy.max_file_bytes {
            return ToolOutcome::error(format!(
                "content is {} bytes, over the {}-byte limit",
                bytes.len(),
                ws.policy.max_file_bytes
            ));
        }
        let path = match ws.workspace.resolve_new(&args.path) {
            Ok(path) => path,
            Err(message) => return ToolOutcome::error(message),
        };
        if let Some(parent) = path.parent()
            && let Err(error) = tokio::fs::create_dir_all(parent).await
        {
            return ToolOutcome::error(format!(
                "could not create parent of {:?}: {error}",
                args.path
            ));
        }
        let result = if args.append {
            use tokio::io::AsyncWriteExt as _;
            match tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .await
            {
                Ok(mut file) => file.write_all(bytes).await,
                Err(error) => Err(error),
            }
        } else {
            tokio::fs::write(&path, bytes).await
        };
        match result {
            Ok(()) => ToolOutcome::ok(format!(
                "{} {} bytes to {}",
                if args.append { "appended" } else { "wrote" },
                bytes.len(),
                args.path
            )),
            Err(error) => ToolOutcome::error(format!("could not write {:?}: {error}", args.path)),
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
            "lightagent-fs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ctx_for(root: &std::path::Path) -> ToolCtx {
        let ws = Workspace::new(root).unwrap();
        let policy = WorkspacePolicy {
            max_file_bytes: 1_000_000,
            allow_terminal: false,
            terminal_timeout: Duration::from_secs(5),
            terminal_allowlist: Vec::new(),
        };
        ToolCtx::new(CancellationToken::new()).with_workspace(WorkspaceContext {
            workspace: Arc::new(ws),
            policy: Arc::new(policy),
        })
    }

    #[tokio::test]
    async fn write_then_read_round_trips_within_the_workspace() {
        let root = scratch();
        let ctx = ctx_for(&root);
        let wrote = FsWrite::new()
            .call(&json!({ "path": "notes/a.txt", "content": "hello" }), &ctx)
            .await;
        assert!(!wrote.is_error, "{}", wrote.content);
        let read = FsRead::new()
            .call(&json!({ "path": "notes/a.txt" }), &ctx)
            .await;
        assert!(!read.is_error);
        assert_eq!(read.content, "hello");
        let listed = FsList::new().call(&json!({ "path": "notes" }), &ctx).await;
        assert!(listed.content.contains("a.txt"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn escaping_paths_are_refused() {
        let root = scratch();
        let ctx = ctx_for(&root);
        let out = FsWrite::new()
            .call(&json!({ "path": "../escape.txt", "content": "x" }), &ctx)
            .await;
        assert!(out.is_error);
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn no_context_is_a_controlled_error() {
        let ctx = ToolCtx::new(CancellationToken::new());
        let out = FsRead::new().call(&json!({ "path": "a" }), &ctx).await;
        assert!(out.is_error);
        assert!(out.content.contains("not enabled"));
    }
}
