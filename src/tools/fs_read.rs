use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::sandbox::allowlist;
use crate::tools::registry::{Tool, ToolCtx, ToolOutput};

const MAX_LIST_ENTRIES: usize = 1024;
const MAX_GLOB_HITS: usize = 512;

// ============================================================== read_file --

pub struct ReadFile;

#[derive(Deserialize)]
struct ReadFileArgs {
    path: PathBuf,
    #[serde(default)]
    start_line: Option<usize>,
    #[serde(default)]
    end_line: Option<usize>,
}

#[async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &'static str { "read_file" }
    fn description(&self) -> &'static str {
        "Read a text file. Optional 1-indexed inclusive start_line/end_line."
    }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":       {"type": "string"},
                "start_line": {"type": "integer", "minimum": 1},
                "end_line":   {"type": "integer", "minimum": 1}
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    async fn run(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput> {
        let a: ReadFileArgs = serde_json::from_value(args)?;
        let resolved = match allowlist::resolve(&ctx.root, &a.path, true) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::err(e.to_string())),
        };
        let raw = match tokio::fs::read(&resolved).await {
            Ok(b) => b,
            Err(e) => return Ok(ToolOutput::err(format!("read failed: {e}"))),
        };
        let text = String::from_utf8_lossy(&raw).into_owned();

        let body = if a.start_line.is_some() || a.end_line.is_some() {
            let start = a.start_line.unwrap_or(1).saturating_sub(1);
            let end = a.end_line.unwrap_or(usize::MAX);
            text.lines()
                .enumerate()
                .filter(|(i, _)| *i >= start && *i < end)
                .map(|(i, l)| format!("{:>5} | {l}", i + 1))
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            text.lines()
                .enumerate()
                .map(|(i, l)| format!("{:>5} | {l}", i + 1))
                .collect::<Vec<_>>()
                .join("\n")
        };

        Ok(ToolOutput::ok(ctx.clamp(body)))
    }
}

// =============================================================== list_dir --

pub struct ListDir;

#[derive(Deserialize)]
struct ListDirArgs {
    path: PathBuf,
}

#[async_trait]
impl Tool for ListDir {
    fn name(&self) -> &'static str { "list_dir" }
    fn description(&self) -> &'static str {
        "List immediate children of a directory under the project root."
    }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
            "additionalProperties": false
        })
    }

    async fn run(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput> {
        let a: ListDirArgs = serde_json::from_value(args)?;
        let dir = match allowlist::resolve(&ctx.root, &a.path, true) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::err(e.to_string())),
        };
        let mut rd = match tokio::fs::read_dir(&dir).await {
            Ok(r) => r,
            Err(e) => return Ok(ToolOutput::err(format!("read_dir failed: {e}"))),
        };

        let mut entries: Vec<String> = Vec::new();
        while let Some(entry) = rd.next_entry().await? {
            if entries.len() >= MAX_LIST_ENTRIES {
                entries.push("… (truncated)".into());
                break;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let meta = entry.metadata().await.ok();
            let kind = match meta.as_ref().map(|m| m.is_dir()) {
                Some(true) => "DIR ",
                Some(false) => "FILE",
                None => "?   ",
            };
            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            entries.push(format!("{kind}  {size:>10}  {name}"));
        }
        entries.sort();
        Ok(ToolOutput::ok(entries.join("\n")))
    }
}

// =================================================================== glob --

pub struct Glob;

#[derive(Deserialize)]
struct GlobArgs {
    pattern: String,
}

#[async_trait]
impl Tool for Glob {
    fn name(&self) -> &'static str { "glob" }
    fn description(&self) -> &'static str {
        "Recursively list files matching a gitignore-aware glob from the project root."
    }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"pattern": {"type": "string"}},
            "required": ["pattern"],
            "additionalProperties": false
        })
    }

    async fn run(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput> {
        let a: GlobArgs = serde_json::from_value(args)?;
        let root = ctx.root.clone();
        let pattern = a.pattern;

        let hits = tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
            use ignore::WalkBuilder;
            let mut matcher = ignore::overrides::OverrideBuilder::new(&root);
            matcher.add(&pattern)?;
            let overrides = matcher.build()?;
            let mut out = Vec::new();
            let walker = WalkBuilder::new(&root).overrides(overrides).build();
            for r in walker {
                let entry = match r { Ok(e) => e, Err(_) => continue };
                if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    if let Ok(rel) = entry.path().strip_prefix(&root) {
                        out.push(rel.display().to_string());
                        if out.len() >= MAX_GLOB_HITS {
                            out.push("… (truncated)".into());
                            break;
                        }
                    }
                }
            }
            Ok(out)
        })
        .await??;

        Ok(ToolOutput::ok(hits.join("\n")))
    }
}

