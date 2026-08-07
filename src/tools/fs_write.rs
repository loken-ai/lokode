use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use similar::TextDiff;

use crate::sandbox::allowlist;
use crate::sandbox::approval::{self, ApprovalDecision, ApprovalRequest};
use crate::tools::registry::{Tool, ToolCtx, ToolOutput};

const MAX_PREVIEW: usize = 16 * 1024;

// ============================================================== write_file --

pub struct WriteFile;

#[derive(Deserialize)]
struct WriteFileArgs {
    path: PathBuf,
    content: String,
}

#[async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &'static str { "write_file" }
    fn description(&self) -> &'static str {
        "Overwrite a file (or create it). Requires user approval."
    }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":    {"type": "string"},
                "content": {"type": "string"}
            },
            "required": ["path", "content"],
            "additionalProperties": false
        })
    }

    async fn run(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput> {
        let a: WriteFileArgs = serde_json::from_value(args)?;
        let target = match allowlist::resolve(&ctx.root, &a.path, false) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::err(e.to_string())),
        };

        let before = tokio::fs::read_to_string(&target).await.unwrap_or_default();
        let diff = unified_diff(&before, &a.content, &target.display().to_string());

        if !ctx.session_approvals.is_always(self.name()).await {
            let decision = approval::ask(
                &ctx.approval,
                ApprovalRequest {
                    tool: self.name().into(),
                    summary: format!("write_file {}  ({} → {} bytes)", target.display(), before.len(), a.content.len()),
                    diff_preview: Some(diff.clone()),
                },
            )
            .await;
            match decision {
                ApprovalDecision::Deny => return Ok(ToolOutput::err("user denied")),
                ApprovalDecision::Always => ctx.session_approvals.mark_always(self.name()).await,
                ApprovalDecision::Once => {}
            }
        }

        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        tokio::fs::write(&target, a.content.as_bytes()).await?;

        Ok(ToolOutput::ok(format!(
            "wrote {} ({} bytes)\n{}",
            target.display(),
            a.content.len(),
            preview(&diff)
        )))
    }
}

// =============================================================== edit_file --

pub struct EditFile;

#[derive(Deserialize)]
struct EditFileArgs {
    path: PathBuf,
    /// Existing substring to replace. Must occur exactly once.
    old_string: String,
    /// Replacement.
    new_string: String,
}

#[async_trait]
impl Tool for EditFile {
    fn name(&self) -> &'static str { "edit_file" }
    fn description(&self) -> &'static str {
        "Replace a unique substring in a file. Fails if old_string is not found or is ambiguous."
    }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":       {"type": "string"},
                "old_string": {"type": "string"},
                "new_string": {"type": "string"}
            },
            "required": ["path", "old_string", "new_string"],
            "additionalProperties": false
        })
    }

    async fn run(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput> {
        let a: EditFileArgs = serde_json::from_value(args)?;
        let target = match allowlist::resolve(&ctx.root, &a.path, true) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::err(e.to_string())),
        };
        let before = tokio::fs::read_to_string(&target).await?;
        let count = before.matches(&a.old_string).count();
        if count == 0 {
            return Ok(ToolOutput::err(format!(
                "old_string not found in {}",
                target.display()
            )));
        }
        if count > 1 {
            return Ok(ToolOutput::err(format!(
                "old_string is ambiguous: found {count} matches in {}; add surrounding context to make it unique",
                target.display()
            )));
        }

        let after = before.replacen(&a.old_string, &a.new_string, 1);
        let diff = unified_diff(&before, &after, &target.display().to_string());

        if !ctx.session_approvals.is_always(self.name()).await {
            let decision = approval::ask(
                &ctx.approval,
                ApprovalRequest {
                    tool: self.name().into(),
                    summary: format!("edit_file {}", target.display()),
                    diff_preview: Some(diff.clone()),
                },
            )
            .await;
            match decision {
                ApprovalDecision::Deny => return Ok(ToolOutput::err("user denied")),
                ApprovalDecision::Always => ctx.session_approvals.mark_always(self.name()).await,
                ApprovalDecision::Once => {}
            }
        }

        tokio::fs::write(&target, after.as_bytes()).await?;
        Ok(ToolOutput::ok(format!(
            "edited {}\n{}",
            target.display(),
            preview(&diff)
        )))
    }
}

// ============================================================= apply_patch --

pub struct ApplyPatch;

#[derive(Deserialize)]
struct ApplyPatchArgs {
    /// Unified diff. File paths in the diff are interpreted relative to the project root.
    unified_diff: String,
}

#[async_trait]
impl Tool for ApplyPatch {
    fn name(&self) -> &'static str { "apply_patch" }
    fn description(&self) -> &'static str {
        "Apply a unified diff that may touch multiple files. Requires user approval."
    }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"unified_diff": {"type": "string"}},
            "required": ["unified_diff"],
            "additionalProperties": false
        })
    }

    async fn run(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput> {
        let a: ApplyPatchArgs = serde_json::from_value(args)?;
        let patches: Vec<diffy::Patch<'_, str>> = match parse_multifile_patch(&a.unified_diff) {
            Ok(ps) => ps,
            Err(e) => return Ok(ToolOutput::err(format!("patch parse failed: {e}"))),
        };
        if patches.is_empty() {
            return Ok(ToolOutput::err("empty patch"));
        }

        if !ctx.session_approvals.is_always(self.name()).await {
            let decision = approval::ask(
                &ctx.approval,
                ApprovalRequest {
                    tool: self.name().into(),
                    summary: format!("apply_patch ({} hunked files)", patches.len()),
                    diff_preview: Some(preview(&a.unified_diff)),
                },
            )
            .await;
            match decision {
                ApprovalDecision::Deny => return Ok(ToolOutput::err("user denied")),
                ApprovalDecision::Always => ctx.session_approvals.mark_always(self.name()).await,
                ApprovalDecision::Once => {}
            }
        }

        // Apply each patch in-memory then write atomically per-file.
        let mut summary = String::new();
        for patch in patches {
            let rel = patch
                .modified()
                .or_else(|| patch.original())
                .unwrap_or("")
                .trim_start_matches("a/")
                .trim_start_matches("b/");
            if rel.is_empty() {
                summary.push_str("skipped hunk without path\n");
                continue;
            }
            let target = match allowlist::resolve(&ctx.root, std::path::Path::new(rel), false) {
                Ok(p) => p,
                Err(e) => {
                    summary.push_str(&format!("{rel}: {e}\n"));
                    continue;
                }
            };
            let before = tokio::fs::read_to_string(&target).await.unwrap_or_default();
            let after = match diffy::apply(&before, &patch) {
                Ok(s) => s,
                Err(e) => {
                    summary.push_str(&format!("{rel}: apply failed: {e}\n"));
                    continue;
                }
            };
            if let Some(parent) = target.parent() {
                tokio::fs::create_dir_all(parent).await.ok();
            }
            tokio::fs::write(&target, after.as_bytes()).await?;
            summary.push_str(&format!("patched {}\n", target.display()));
        }

        Ok(ToolOutput::ok(summary))
    }
}

// -------------------------------------------------------------- helpers -----

fn unified_diff(before: &str, after: &str, path: &str) -> String {
    let diff = TextDiff::from_lines(before, after);
    diff.unified_diff()
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string()
}

fn preview(s: &str) -> String {
    if s.len() > MAX_PREVIEW {
        let mut out = s.chars().take(MAX_PREVIEW).collect::<String>();
        out.push_str("\n… (truncated)");
        out
    } else {
        s.to_string()
    }
}

/// Split a concatenated unified diff into per-file patches using `--- ` boundaries
/// (with a `+++` companion). diffy expects one patch per parse call.
fn parse_multifile_patch(src: &str) -> Result<Vec<diffy::Patch<'_, str>>, diffy::ParsePatchError> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    let bytes = src.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let line_start = i;
        // Find end of line
        while i < bytes.len() && bytes[i] != b'\n' { i += 1; }
        let line_end = i;
        if i < bytes.len() { i += 1; }

        let line = &src[line_start..line_end];
        if line.starts_with("--- ") {
            if let Some(s) = start {
                let chunk = &src[s..line_start];
                out.push(diffy::Patch::from_str(chunk)?);
            }
            start = Some(line_start);
        }
    }
    if let Some(s) = start {
        out.push(diffy::Patch::from_str(&src[s..])?);
    }
    Ok(out)
}
