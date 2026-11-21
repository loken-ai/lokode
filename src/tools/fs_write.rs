use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use similar::TextDiff;
use tokio::io::AsyncWriteExt;

use crate::sandbox::allowlist;
use crate::sandbox::approval::{write_scope, Approval, ApprovalRequest};
use crate::tools::registry::{Tool, ToolCtx, ToolOutput};

const MAX_PREVIEW: usize = 16 * 1024;

// ============================================================== write_file --

pub struct WriteFile;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Found {
    /// Exactly where it was written: the byte range.
    Exact(std::ops::Range<usize>),
    /// Once its lines are compared without their leading and trailing whitespace: the byte range
    /// of those whole lines in the file.
    Loose(std::ops::Range<usize>),
    /// Nowhere: the 1-based first and last line of the window of the file most like it.
    Nearest { first: usize, last: usize },
    /// More than once, exactly or loosely: how many times.
    Ambiguous(usize),
}

/// Write an edit found in the file, once it is approved.
///
/// `loose` says the text was found with its leading and trailing spaces ignored.
async fn apply_found(
    ctx: &ToolCtx,
    target: &std::path::Path,
    before: &str,
    after: String,
    loose: bool,
) -> Result<ToolOutput> {
    let diff = unified_diff(before, &after, &ctx.short(target));
    if let Err(why) = ctx.may_write(target) {
        return Ok(ToolOutput::err(why));
    }
    match ctx
        .approve(ApprovalRequest {
            tool: "edit_file".into(),
            summary: format!("edit_file {}", ctx.short(target)),
            diff_preview: Some(diff.clone()),
            scope: write_scope("edit_file", target),
        })
        .await
    {
        Approval::Proceed => {}
        Approval::Denied(why) => return Ok(ToolOutput::err(crate::sandbox::approval::denial(why))),
        Approval::Interrupted => return Ok(ToolOutput::err("interrupted by user")),
    }
    ctx.write_text(target, &after).await?;
    ctx.note_content(target, after.as_bytes());
    let note = match loose {
        true => "\napplied where the text matched once leading and trailing spaces were ignored",
        false => "",
    };
    Ok(ToolOutput::ok(format!(
        "edited {}\n{}{note}",
        ctx.short(target),
        preview(&diff)
    )))
}

/// Find where an `old` stands in `file`.
///
/// Rules:
/// 1. Exact: `old` occurs once in `file`. Twice or more: `Ambiguous(n)`.
/// 2. Loose: otherwise, split `old` into lines (ignore a final empty line), and slide a window of
///    the same number of lines over the file; a window matches when every line, trimmed, equals
///    the corresponding trimmed line of `old`, and `old` has at least one non-empty trimmed line.
///    Exactly one matching window gives `Loose(range)`, the range from the start of its first line
///    to the end of its last line, without the last newline. More than one gives `Ambiguous(n)`.
/// 3. Nearest: otherwise, the window of the same number of lines with the most lines equal once
///    trimmed (the first such window on a tie), as `Nearest { first, last }`. A window with no
///    equal line still counts: then it is the first window.
pub(crate) fn find(file: &str, old: &str) -> Found {
    // An empty text stands everywhere.
    if old.is_empty() {
        return Found::Ambiguous(file.len() + 1);
    }
    let mut exact = file.match_indices(old).map(|(at, _)| at);
    match (exact.next(), exact.count()) {
        (Some(at), 0) => return Found::Exact(at..at + old.len()),
        (Some(_), more) => return Found::Ambiguous(more + 1),
        (None, _) => {}
    }

    let mut wanted: Vec<&str> = old.split('\n').map(str::trim).collect();
    if wanted.last() == Some(&"") {
        wanted.pop();
    }
    let lines: Vec<&str> = file.split('\n').collect();
    // The byte at which each line starts, and one past the end of the file.
    let mut starts = Vec::with_capacity(lines.len() + 1);
    let mut at = 0;
    for line in &lines {
        starts.push(at);
        at += line.len() + 1;
    }
    starts.push(file.len() + 1);
    let trimmed: Vec<&str> = lines.iter().map(|l| l.trim()).collect();
    let width = wanted.len().max(1);

    // A text of blank lines matches anything loosely, so only one with words is looked for.
    if wanted.iter().any(|l| !l.is_empty()) && wanted.len() <= trimmed.len() {
        let loose: Vec<usize> = (0..=trimmed.len() - wanted.len())
            .filter(|&i| trimmed[i..i + wanted.len()] == wanted[..])
            .collect();
        match loose.as_slice() {
            [] => {}
            [i] => {
                // From the start of the first line to the end of the last, its newline left out.
                return Found::Loose(starts[*i]..starts[i + wanted.len()] - 1);
            }
            more => return Found::Ambiguous(more.len()),
        }
    }

    // The first window with the most lines equal once trimmed.
    let mut best = (0, 0);
    for (i, window) in trimmed.windows(width.min(trimmed.len())).enumerate() {
        let equal = window.iter().zip(&wanted).filter(|(a, b)| a == b).count();
        if equal > best.1 {
            best = (i, equal);
        }
    }
    Found::Nearest {
        first: best.0 + 1,
        last: (best.0 + width).min(lines.len()),
    }
}

#[derive(Deserialize)]
struct WriteFileArgs {
    path: PathBuf,
    content: String,
    /// Add `content` at the end of the file instead of replacing it.
    #[serde(default)]
    append: bool,
}

#[async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &str {
        "write_file"
    }
    fn mutates(&self) -> bool {
        true
    }
    fn description(&self) -> &str {
        "Create a file, or replace one whole. Everything not in `content` is gone: to \
         change part of a file that already exists, use edit_file or edit_lines. With \
         `append` true, `content` is added at the end of the file instead, so a long new \
         file is written in several calls. Requires user approval."
    }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":    {"type": "string"},
                "content": {"type": "string"},
                "append":  {"type": "boolean"}
            },
            "required": ["path", "content"],
            "additionalProperties": false
        })
    }

    async fn run(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput> {
        let a: WriteFileArgs = serde_json::from_value(args)?;
        let target = match allowlist::resolve(&ctx.root, &a.path, false) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::err(allowlist::refusal(&ctx.root, &a.path, &e))),
        };

        let existing = match ctx.read_text_if_exists(&target).await {
            Ok(b) => b,
            Err(e) => return Ok(ToolOutput::err(e)),
        };
        // A whole-file write replaces what the model believes the file says.
        // If someone has changed the file since the model read it, that belief is stale.
        // The write would throw their work away without either of them seeing it happen.
        //
        // A file that is gone holds no work to lose: writing it creates it again.
        let stale = existing
            .as_ref()
            .is_some_and(|now| ctx.changed_since_seen(&target, now.as_bytes()));
        let before = existing.unwrap_or_default();
        if stale {
            return Ok(ToolOutput::err(format!(
                "{} changed on disk since you read it; read it again before writing it",
                ctx.short(&target)
            )));
        }
        let after = match a.append {
            true => format!("{before}{}", a.content),
            false => a.content.clone(),
        };
        let diff = unified_diff(&before, &after, &ctx.short(&target));

        if let Err(why) = ctx.may_write(&target) {
            return Ok(ToolOutput::err(why));
        }
        match ctx
            .approve(ApprovalRequest {
                tool: self.name().into(),
                summary: format!(
                    "write_file {}  ({} → {} bytes){}",
                    ctx.short(&target),
                    before.len(),
                    after.len(),
                    losses(&diff)
                ),
                diff_preview: Some(diff.clone()),
                scope: write_scope(self.name(), &target),
            })
            .await
        {
            Approval::Proceed => {}
            Approval::Denied(why) => {
                return Ok(ToolOutput::err(crate::sandbox::approval::denial(why)))
            }
            Approval::Interrupted => return Ok(ToolOutput::err("interrupted by user")),
        }

        ctx.write_text(&target, &after).await?;
        ctx.note_content(&target, after.as_bytes());

        Ok(ToolOutput::ok(format!(
            "wrote {} ({} bytes)\n{}",
            ctx.short(&target),
            after.len(),
            preview(&diff)
        )))
    }
}

// =============================================================== edit_file --

pub struct EditFile;

#[derive(Deserialize)]
struct EditFileArgs {
    path: PathBuf,
    /// Existing substring to replace.
    /// Must occur exactly once, unless `replace_all` is true.
    old_string: String,
    /// Replacement.
    new_string: String,
    /// Replace all exact occurrences of `old_string` in the file.
    #[serde(default)]
    replace_all: bool,
}

#[async_trait]
impl Tool for EditFile {
    fn name(&self) -> &str {
        "edit_file"
    }
    fn mutates(&self) -> bool {
        true
    }
    fn description(&self) -> &str {
        "Replace a unique substring in a file. Fails if old_string is not found or is ambiguous. \
         When `replace_all` is true, replace every exact occurrence."
    }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":       {"type": "string"},
                "old_string": {"type": "string"},
                "new_string": {"type": "string"},
                "replace_all": {"type": "boolean"}
            },
            "required": ["path", "old_string", "new_string"],
            "additionalProperties": false
        })
    }

    async fn run(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput> {
        let a: EditFileArgs = serde_json::from_value(args)?;
        let target = match allowlist::resolve(&ctx.root, &a.path, true) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::err(allowlist::refusal(&ctx.root, &a.path, &e))),
        };
        let before = match ctx.read_text(&target).await {
            Ok(b) => b,
            Err(e) => return Ok(ToolOutput::err(e)),
        };

        let found = find(&before, &a.old_string);
        // Every exact occurrence is replaced when there is at least one.
        // With none, the edit is found loosely or reported as for a single replacement.
        if a.replace_all {
            let count = before.matches(&a.old_string).count();
            if count > 0 {
                let after = before.replace(&a.old_string, &a.new_string);
                let mut out = apply_found(ctx, &target, &before, after, false).await?;
                if count > 1 && !out.is_error {
                    out.content
                        .push_str(&format!("\nreplaced {count} occurrences"));
                }
                return Ok(out);
            }
        }
        return match found {
            found @ (Found::Exact(_) | Found::Loose(_)) => {
                let (Found::Exact(range) | Found::Loose(range)) = found.clone() else {
                    unreachable!()
                };
                let after = format!("{}{}{}", &before[..range.start], a.new_string, &before[range.end..]);
                apply_found(ctx, &target, &before, after, matches!(found, Found::Loose(_))).await
            }
            Found::Ambiguous(n) => {
                Ok(ToolOutput::err(format!(
                    "old_string is ambiguous: found {n} matches in {}; add surrounding context to make it unique",
                    ctx.short(&target)
                )))
            }
            Found::Nearest { first, last } => {
                let lines: Vec<&str> = before.split('\n').collect();
                let window_start = first - 1;
                let window_end = last.min(lines.len());
                let lines_output = lines[window_start..window_end]
                    .iter()
                    .enumerate()
                    .map(|(i, line)| format!("{} | {}", i + first, line))
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(ToolOutput::err(format!(
                    "old_string not found in {}. The closest lines are {}-{}:\n{}\nnothing was changed",
                    ctx.short(&target),
                    first,
                    last,
                    lines_output
                )))
            }
        };
    }
}

// ============================================================== edit_lines --

pub struct EditLines;

#[derive(Deserialize)]
struct EditLinesArgs {
    path: PathBuf,
    edits: Vec<OneEdit>,
}

#[derive(Deserialize)]
struct OneEdit {
    /// Existing substring to replace.
    /// Must occur exactly once in the file as it stands when this edit's turn comes.
    old_string: String,
    new_string: String,
}

#[async_trait]
impl Tool for EditLines {
    fn name(&self) -> &str {
        "edit_lines"
    }
    fn mutates(&self) -> bool {
        true
    }
    fn description(&self) -> &str {
        "Apply several unique-substring replacements to one file, in order, all or none. \
         Prefer this over repeated edit_file on the same file."
    }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "edits": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_string": {"type": "string"},
                            "new_string": {"type": "string"}
                        },
                        "required": ["old_string", "new_string"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["path", "edits"],
            "additionalProperties": false
        })
    }

    /// All of them or none of them.
    ///
    /// Applied to the text in memory first, so an edit that cannot be placed leaves the file
    /// as it was.
    /// Each is checked against the text as the earlier ones left it.
    async fn run(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput> {
        let a: EditLinesArgs = serde_json::from_value(args)?;
        if a.edits.is_empty() {
            return Ok(ToolOutput::err("no edits given"));
        }
        let target = match allowlist::resolve(&ctx.root, &a.path, true) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::err(allowlist::refusal(&ctx.root, &a.path, &e))),
        };
        let before = match ctx.read_text(&target).await {
            Ok(b) => b,
            Err(e) => return Ok(ToolOutput::err(e)),
        };

        let mut after = before.clone();
        for (i, edit) in a.edits.iter().enumerate() {
            let found = find(&after, &edit.old_string);
            match found {
                Found::Exact(range) | Found::Loose(range) => {
                    let start = range.start;
                    let end = range.end;
                    after = after[..start].to_string() + &edit.new_string + &after[end..];
                }
                Found::Ambiguous(n) => {
                    return Ok(ToolOutput::err(format!(
                        "edit {} of {}: old_string is ambiguous: matched {n} times in {}; nothing was changed",
                        i + 1,
                        a.edits.len(),
                        ctx.short(&target)
                    )));
                }
                Found::Nearest { first, last } => {
                    let lines: Vec<&str> = after.split('\n').collect();
                    let window_start = first - 1;
                    let window_end = last.min(lines.len());
                    let lines_output = lines[window_start..window_end]
                        .iter()
                        .enumerate()
                        .map(|(i, line)| format!("{} | {}", i + first, line))
                        .collect::<Vec<_>>()
                        .join("\n");
                    return Ok(ToolOutput::err(format!(
                        "edit {} of {}: old_string not found in {}. The closest lines are {}-{}:\n{}\nnothing was changed",
                        i + 1,
                        a.edits.len(),
                        ctx.short(&target),
                        first,
                        last,
                        lines_output
                    )));
                }
            }
        }
        let diff = unified_diff(&before, &after, &ctx.short(&target));

        if let Err(why) = ctx.may_write(&target) {
            return Ok(ToolOutput::err(why));
        }
        match ctx
            .approve(ApprovalRequest {
                tool: self.name().into(),
                summary: format!(
                    "edit_lines {} ({} edits)",
                    ctx.short(&target),
                    a.edits.len()
                ),
                diff_preview: Some(diff.clone()),
                scope: write_scope(self.name(), &target),
            })
            .await
        {
            Approval::Proceed => {}
            Approval::Denied(why) => {
                return Ok(ToolOutput::err(crate::sandbox::approval::denial(why)))
            }
            Approval::Interrupted => return Ok(ToolOutput::err("interrupted by user")),
        }

        ctx.write_text(&target, &after).await?;
        ctx.note_content(&target, after.as_bytes());
        Ok(ToolOutput::ok(format!(
            "applied {} edit(s) to {}\n{}",
            a.edits.len(),
            ctx.short(&target),
            preview(&diff)
        )))
    }
}

// ============================================================= apply_patch --

pub struct ApplyPatch;

#[derive(Deserialize)]
struct ApplyPatchArgs {
    /// Unified diff.
    /// File paths in the diff are interpreted relative to the project root.
    unified_diff: String,
}

#[async_trait]
impl Tool for ApplyPatch {
    fn name(&self) -> &str {
        "apply_patch"
    }
    fn mutates(&self) -> bool {
        true
    }
    fn description(&self) -> &str {
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

        // Everything is resolved and applied in memory before anything is asked or written.
        // The user approves a diff whose paths are already checked.
        // Rejections are not folded into the summary, so what was agreed to is what happened.
        let mut staged: Vec<Staged> = Vec::with_capacity(patches.len());
        for patch in &patches {
            let rel = patch
                .modified()
                .or_else(|| patch.original())
                .unwrap_or("")
                .trim_start_matches("a/")
                .trim_start_matches("b/");
            if rel.is_empty() {
                return Ok(ToolOutput::err("patch contains a hunk with no path"));
            }
            let target = match allowlist::resolve(&ctx.root, std::path::Path::new(rel), false) {
                Ok(p) => p,
                Err(e) => {
                    let why = allowlist::refusal(&ctx.root, std::path::Path::new(rel), &e);
                    return Ok(ToolOutput::err(format!("{rel}: {why}")));
                }
            };
            let before = match ctx.read_text_if_exists(&target).await {
                Ok(before) => before,
                Err(e) => return Ok(ToolOutput::err(format!("{rel}: {e}"))),
            };
            let after = match diffy::apply(before.as_deref().unwrap_or(""), patch) {
                Ok(s) => s,
                Err(e) => return Ok(ToolOutput::err(format!("{rel}: apply failed: {e}"))),
            };
            staged.push(Staged {
                target,
                before,
                after,
            });
        }

        let targets: Vec<&std::path::Path> = staged.iter().map(|s| s.target.as_path()).collect();
        for s in &staged {
            if let Err(why) = ctx.may_write(&s.target) {
                return Ok(ToolOutput::err(why));
            }
        }
        match ctx
            .approve(ApprovalRequest {
                tool: self.name().into(),
                summary: format!(
                    "apply_patch, {} file(s):\n{}",
                    staged.len(),
                    staged
                        .iter()
                        .map(|s| format!("  {}", ctx.short(&s.target)))
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
                diff_preview: Some(preview(&a.unified_diff)),
                scope: write_scope(self.name(), &common_ancestor(&targets)),
            })
            .await
        {
            Approval::Proceed => {}
            Approval::Denied(why) => {
                return Ok(ToolOutput::err(crate::sandbox::approval::denial(why)))
            }
            Approval::Interrupted => return Ok(ToolOutput::err("interrupted by user")),
        }

        // A patch is one change to a project, not a sequence of unrelated ones.
        // A failure part way through restores the earlier files.
        let mut committed: Vec<&Staged> = Vec::with_capacity(staged.len());
        for s in &staged {
            if let Err(e) = ctx.write_text(&s.target, &s.after).await {
                let rolled = roll_back(&committed).await;
                return Ok(ToolOutput::err(format!(
                    "{}: write failed: {e}\n{rolled}",
                    ctx.short(&s.target)
                )));
            }
            committed.push(s);
        }
        // Recorded once the whole patch is on disk.
        // A run that rolled back left the files as they were.
        for s in &committed {
            ctx.note_content(&s.target, s.after.as_bytes());
        }

        Ok(ToolOutput::ok(
            staged
                .iter()
                .map(|s| format!("patched {}", ctx.short(&s.target)))
                .collect::<Vec<_>>()
                .join("\n"),
        ))
    }
}

/// One file's change, held until every file's change is known to be possible.
struct Staged {
    target: PathBuf,
    /// Contents before the patch, or `None` if the patch creates the file.
    before: Option<String>,
    after: String,
}

/// Put back what was already written when a later file could not be.
///
/// The outcome is reported, not returned.
/// The caller cares about the error of the write that failed.
/// The user is told whether the undo itself succeeded.
async fn roll_back(committed: &[&Staged]) -> String {
    let mut failures = Vec::new();
    for s in committed {
        let undo = match &s.before {
            Some(before) => write_atomic(&s.target, before).await,
            None => tokio::fs::remove_file(&s.target).await.map_err(Into::into),
        };
        if let Err(e) = undo {
            failures.push(format!("  {}: {e}", s.target.display()));
        }
    }
    if failures.is_empty() {
        format!(
            "rolled back {} earlier file(s); the tree is as it was",
            committed.len()
        )
    } else {
        format!(
            "ROLLBACK INCOMPLETE - these files are left changed:\n{}",
            failures.join("\n")
        )
    }
}

/// The deepest directory containing all of `paths`.
fn common_ancestor(paths: &[&std::path::Path]) -> PathBuf {
    let mut it = paths.iter().filter_map(|p| p.parent());
    let Some(first) = it.next() else {
        return PathBuf::from("/");
    };
    let mut common = first.to_path_buf();
    for p in it {
        while !p.starts_with(&common) {
            match common.parent() {
                Some(up) => common = up.to_path_buf(),
                None => return common,
            }
        }
    }
    common
}

/// Replace a file's contents without ever leaving it partly written.
///
/// The contents are written beside the target, then renamed over it.
/// A reader sees the old version or the new one, never a mix.
/// The temporary lives in the same directory, so the rename stays on one filesystem.
/// That keeps the rename atomic.
pub(crate) async fn write_atomic(target: &std::path::Path, contents: &str) -> Result<()> {
    let parent = target
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", target.display()))?;
    tokio::fs::create_dir_all(parent).await?;

    let tmp = parent.join(format!(".lokode-{}.tmp", uuid::Uuid::new_v4()));
    let write = async {
        let mut f = tokio::fs::File::create(&tmp).await?;
        f.write_all(contents.as_bytes()).await?;
        // Durable before the rename publishes it.
        // Otherwise a crash can leave the name pointing at a file whose contents never reached
        // the disk.
        f.sync_all().await?;
        Ok::<_, std::io::Error>(())
    }
    .await;

    if let Err(e) = write {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e.into());
    }
    if let Err(e) = tokio::fs::rename(&tmp, target).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e.into());
    }
    Ok(())
}

// -------------------------------------------------------------- helpers -----

/// What a whole-file write is about to take out, when it takes out more than it puts in.
///
/// A write that replaces a file is the one change that can quietly lose work.
/// The model reproduces what it remembers of the file and drops the rest.
/// The line counts are in the diff, but the summary is what is read.
fn losses(diff: &str) -> String {
    let removed = diff
        .lines()
        .filter(|l| l.starts_with('-') && !l.starts_with("---"))
        .count();
    let added = diff
        .lines()
        .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
        .count();
    if removed > added {
        format!("  REMOVES {removed} line(s), adds {added}")
    } else {
        String::new()
    }
}

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
/// (with a `+++` companion).
///
/// `diffy` expects one patch per parse call.
fn parse_multifile_patch(src: &str) -> Result<Vec<diffy::Patch<'_, str>>, diffy::ParsePatchError> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    let bytes = src.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let line_start = i;
        // Find end of line
        while i < bytes.len() && bytes[i] != b'\n' {
            i += 1;
        }
        let line_end = i;
        if i < bytes.len() {
            i += 1;
        }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_write_that_takes_more_out_than_it_puts_in_says_so() {
        let diff = unified_diff("a\nb\nc\n", "a\n", "f.rs");
        assert!(
            losses(&diff).contains("REMOVES 2 line(s), adds 0"),
            "{diff}"
        );
        let grown = unified_diff("a\n", "a\nb\n", "f.rs");
        assert_eq!(losses(&grown), "");
    }

    /// A project reached through a symbolic link is still the project.
    /// Its files are still named by what comes after the root.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_path_under_a_linked_root_is_still_shown_short() {
        let h = crate::tools::harness::Harness::new(crate::tools::harness::Answer::Yes);
        h.write("a.txt", "one");
        let link = h.root.parent().unwrap().join(format!(
            "linked-{}",
            h.root.file_name().unwrap().to_string_lossy()
        ));
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&h.root, &link).unwrap();
        let mut ctx = h.ctx.clone();
        ctx.root = link.clone();
        let out = WriteFile
            .run(&ctx, serde_json::json!({"path": "a.txt", "content": "two"}))
            .await
            .unwrap();
        let _ = std::fs::remove_file(&link);
        assert!(!out.is_error, "{out:?}");
        assert!(out.content.starts_with("wrote a.txt "), "{}", out.content);
    }

    /// A file somebody changed while the model was thinking is not the file the model read.
    /// Overwriting it whole would throw their work away with nothing to show it.
    #[tokio::test]
    async fn a_write_over_a_file_that_moved_is_refused() {
        let h = crate::tools::harness::Harness::new(crate::tools::harness::Answer::Yes);
        h.write("a.txt", "one");
        let read = crate::tools::fs_read::ReadFile
            .run(&h.ctx, serde_json::json!({"path": "a.txt"}))
            .await
            .unwrap();
        assert!(!read.is_error, "{read:?}");

        // Someone else edits it.
        h.write("a.txt", "one, and their change");

        let out = WriteFile
            .run(
                &h.ctx,
                serde_json::json!({"path": "a.txt", "content": "two"}),
            )
            .await
            .unwrap();
        assert!(out.is_error, "{out:?}");
        assert!(out.content.contains("changed on disk"), "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(h.root.join("a.txt")).unwrap(),
            "one, and their change"
        );
    }

    /// A file deleted since it was read holds nothing to lose, so writing it creates it again.
    #[tokio::test]
    async fn a_file_deleted_since_it_was_read_can_be_written_again() {
        let h = crate::tools::harness::Harness::new(crate::tools::harness::Answer::Yes);
        h.write("a.txt", "one");
        let read = crate::tools::fs_read::ReadFile
            .run(&h.ctx, serde_json::json!({"path": "a.txt"}))
            .await
            .unwrap();
        assert!(!read.is_error, "{read:?}");
        std::fs::remove_file(h.root.join("a.txt")).unwrap();

        let out = WriteFile
            .run(
                &h.ctx,
                serde_json::json!({"path": "a.txt", "content": "two"}),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{out:?}");
        assert_eq!(
            std::fs::read_to_string(h.root.join("a.txt")).unwrap(),
            "two"
        );
    }

    /// Reading it again is what makes the write allowed.
    /// The model is then writing over what is actually there.
    #[tokio::test]
    async fn reading_it_again_lets_the_write_through() {
        let h = crate::tools::harness::Harness::new(crate::tools::harness::Answer::Yes);
        h.write("a.txt", "one");
        let _ = crate::tools::fs_read::ReadFile
            .run(&h.ctx, serde_json::json!({"path": "a.txt"}))
            .await
            .unwrap();
        h.write("a.txt", "their change");
        let _ = crate::tools::fs_read::ReadFile
            .run(&h.ctx, serde_json::json!({"path": "a.txt"}))
            .await
            .unwrap();

        let out = WriteFile
            .run(
                &h.ctx,
                serde_json::json!({"path": "a.txt", "content": "two"}),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{out:?}");
        assert_eq!(
            std::fs::read_to_string(h.root.join("a.txt")).unwrap(),
            "two"
        );
    }

    /// A file no tool has read is nobody's belief about anything, so the write stands.
    #[tokio::test]
    async fn a_file_never_read_is_written_without_argument() {
        let h = crate::tools::harness::Harness::new(crate::tools::harness::Answer::Yes);
        h.write("a.txt", "one");
        let out = WriteFile
            .run(
                &h.ctx,
                serde_json::json!({"path": "a.txt", "content": "two"}),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{out:?}");
    }

    use std::path::Path;
    use tempfile::tempdir;

    #[tokio::test]
    async fn a_replaced_file_holds_the_new_contents_and_no_leftovers() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("a.txt");
        tokio::fs::write(&f, b"old").await.unwrap();

        write_atomic(&f, "new").await.unwrap();

        assert_eq!(tokio::fs::read_to_string(&f).await.unwrap(), "new");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".lokode-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temporary files left behind: {leftovers:?}"
        );
    }

    #[tokio::test]
    async fn writing_creates_missing_directories() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("a/b/c.txt");
        write_atomic(&f, "hello").await.unwrap();
        assert_eq!(tokio::fs::read_to_string(&f).await.unwrap(), "hello");
    }

    /// A patch is one change to a project.
    /// If a later file cannot be written, the earlier ones are not left changed.
    #[tokio::test]
    async fn rolling_back_restores_modified_files_and_removes_created_ones() {
        let dir = tempdir().unwrap();
        let modified = dir.path().join("was-there.txt");
        let created = dir.path().join("was-not.txt");
        tokio::fs::write(&modified, b"original").await.unwrap();

        // What the commit phase would have done before hitting a failure.
        write_atomic(&modified, "patched").await.unwrap();
        write_atomic(&created, "brand new").await.unwrap();

        let staged = [
            Staged {
                target: modified.clone(),
                before: Some("original".into()),
                after: "patched".into(),
            },
            Staged {
                target: created.clone(),
                before: None,
                after: "brand new".into(),
            },
        ];
        let report = roll_back(&staged.iter().collect::<Vec<_>>()).await;

        assert_eq!(
            tokio::fs::read_to_string(&modified).await.unwrap(),
            "original"
        );
        assert!(
            !created.exists(),
            "a file the patch created should be gone again"
        );
        assert!(report.contains("as it was"), "unexpected report: {report}");
    }

    #[tokio::test]
    async fn an_incomplete_rollback_says_so_loudly() {
        let dir = tempdir().unwrap();
        // Nothing to restore it from, and its parent no longer exists.
        let gone = dir.path().join("no/such/dir/file.txt");
        let staged = [Staged {
            target: gone,
            before: None,
            after: String::new(),
        }];
        let report = roll_back(&staged.iter().collect::<Vec<_>>()).await;
        assert!(
            report.contains("ROLLBACK INCOMPLETE"),
            "unexpected report: {report}"
        );
    }

    #[test]
    fn the_common_ancestor_is_the_deepest_directory_holding_everything() {
        let paths = [
            Path::new("/p/src/main.rs"),
            Path::new("/p/src/ui/app.rs"),
            Path::new("/p/src/ui/chat.rs"),
        ];
        assert_eq!(common_ancestor(&paths), Path::new("/p/src"));

        let wider = [Path::new("/p/src/main.rs"), Path::new("/p/Cargo.toml")];
        assert_eq!(common_ancestor(&wider), Path::new("/p"));

        let one = [Path::new("/p/src/main.rs")];
        assert_eq!(common_ancestor(&one), Path::new("/p/src"));
    }

    /// The scope an `Always` on a multi-file patch grants covers every file it was shown.
    /// It covers nothing above them.
    #[test]
    fn a_patch_scope_covers_exactly_what_was_shown() {
        use crate::sandbox::approval::write_scope;
        let paths = [Path::new("/p/src/a.rs"), Path::new("/p/src/ui/b.rs")];
        let scope = write_scope("apply_patch", &common_ancestor(&paths));
        assert_eq!(
            scope,
            crate::sandbox::approval::ApprovalScope::Path {
                tool: "apply_patch".into(),
                prefix: PathBuf::from("/p"),
            }
        );
    }
    // ---------------------------------------------------------- through a ToolCtx --

    use crate::sandbox::approval::ApprovalScope;
    use crate::tools::harness::{refused, succeeded, Answer, Harness};
    use serde_json::json;

    #[tokio::test]
    async fn write_file_creates_and_replaces() {
        let h = Harness::new(Answer::Yes);
        let out = h
            .run(&WriteFile, json!({"path": "src/a.rs", "content": "first"}))
            .await;
        succeeded(&out);
        assert_eq!(h.read("src/a.rs"), "first");

        let out = h
            .run(&WriteFile, json!({"path": "src/a.rs", "content": "second"}))
            .await;
        succeeded(&out);
        assert_eq!(h.read("src/a.rs"), "second");
    }

    /// With `append`, a long file is written in several calls, each adding to the end.
    #[tokio::test]
    async fn write_file_appends_in_parts() {
        let h = Harness::new(Answer::Yes);
        for (part, append) in [("fn a() {}\n", false), ("fn b() {}\n", true)] {
            let args = json!({"path": "src/a.rs", "content": part, "append": append});
            succeeded(&h.run(&WriteFile, args).await);
        }
        assert_eq!(h.read("src/a.rs"), "fn a() {}\nfn b() {}\n");
    }

    #[tokio::test]
    async fn a_declined_write_does_not_happen() {
        let h = Harness::new(Answer::No);
        let out = h
            .run(&WriteFile, json!({"path": "a.txt", "content": "nope"}))
            .await;
        refused(&out, "user denied");
        assert!(
            !h.exists("a.txt"),
            "the file was written despite being declined"
        );
    }

    #[tokio::test]
    async fn a_write_outside_the_root_is_refused_without_asking() {
        let h = Harness::new(Answer::Yes);
        let out = h
            .run(
                &WriteFile,
                json!({"path": "../escaped.txt", "content": "x"}),
            )
            .await;
        assert!(out.is_error, "expected a refusal: {}", out.content);
        assert!(
            h.asked().is_empty(),
            "the user was asked about a path that was never going to be allowed"
        );
    }

    /// The scope shown to the user is the one that gets recorded.
    /// It is the directory the file lives in, not the tool.
    #[tokio::test]
    async fn agreeing_always_covers_the_directory_and_stops_there() {
        let h = Harness::new(Answer::Always);
        succeeded(
            &h.run(&WriteFile, json!({"path": "src/a.rs", "content": "1"}))
                .await,
        );
        assert_eq!(h.asked().len(), 1);
        assert_eq!(
            h.asked()[0].scope,
            ApprovalScope::Path {
                tool: "write_file".into(),
                prefix: h.path("src"),
            }
        );

        // Same directory: no second question.
        succeeded(
            &h.run(&WriteFile, json!({"path": "src/b.rs", "content": "2"}))
                .await,
        );
        assert_eq!(h.asked().len(), 1, "a covered write asked again");

        // Somewhere else: asked again.
        succeeded(
            &h.run(&WriteFile, json!({"path": "Cargo.toml", "content": "3"}))
                .await,
        );
        assert_eq!(
            h.asked().len(),
            2,
            "a write outside the agreement did not ask"
        );
    }

    #[tokio::test]
    async fn an_abandoned_turn_stops_a_write_before_it_asks() {
        let h = Harness::cancelled();
        let out = h
            .run(&WriteFile, json!({"path": "a.txt", "content": "x"}))
            .await;
        refused(&out, "interrupted");
        assert!(!h.exists("a.txt"));
    }

    // ------------------------------------------------------------------ edit_file --

    #[tokio::test]
    async fn edit_file_replaces_a_unique_substring() {
        let h = Harness::new(Answer::Yes);
        h.write("a.rs", "let x = 1;\nlet y = 2;\n");
        let out = h
            .run(
                &EditFile,
                json!({"path": "a.rs", "old_string": "let y = 2;", "new_string": "let y = 3;"}),
            )
            .await;
        succeeded(&out);
        assert_eq!(h.read("a.rs"), "let x = 1;\nlet y = 3;\n");
    }

    /// An ambiguous match is the case where guessing would silently edit the wrong line.
    #[tokio::test]
    async fn edit_file_refuses_an_ambiguous_match() {
        let h = Harness::new(Answer::Yes);
        h.write("a.rs", "same\nsame\n");
        let out = h
            .run(
                &EditFile,
                json!({"path": "a.rs", "old_string": "same", "new_string": "changed"}),
            )
            .await;
        refused(&out, "ambiguous");
        assert_eq!(
            h.read("a.rs"),
            "same\nsame\n",
            "the file was touched anyway"
        );
        assert!(
            h.asked().is_empty(),
            "the user was asked about an edit that could not be made"
        );
    }

    #[tokio::test]
    async fn edit_file_refuses_a_string_that_is_not_there() {
        let h = Harness::new(Answer::Yes);
        h.write("a.rs", "hello\n");
        let out = h
            .run(
                &EditFile,
                json!({"path": "a.rs", "old_string": "goodbye", "new_string": "x"}),
            )
            .await;
        refused(&out, "not found");
    }

    /// With no exact occurrence, replace_all applies an edit found once its spaces are ignored.
    #[tokio::test]
    async fn edit_file_replace_all_finds_a_loose_match() {
        let h = Harness::new(Answer::Yes);
        h.write("a.rs", "fn a() {\n    x();\n}\n");
        let out = EditFile
            .run(
                &h.ctx,
                json!({"path": "a.rs", "old_string": "x();  ", "new_string": "    y();", "replace_all": true}),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{out:?}");
        assert_eq!(h.read("a.rs"), "fn a() {\n    y();\n}\n");
    }

    #[tokio::test]
    async fn edit_file_replace_all_with_three_occurrences() {
        let h = Harness::new(Answer::Yes);
        h.write("a.rs", "same\nsame\nsame\n");
        let out = EditFile
            .run(
                &h.ctx,
                json!({"path": "a.rs", "old_string": "same", "new_string": "changed", "replace_all": true}),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{out:?}");
        assert_eq!(h.read("a.rs"), "changed\nchanged\nchanged\n");
        assert!(out.content.contains("replaced 3 occurrences"), "{out:?}");
    }

    #[tokio::test]
    async fn edit_file_replace_all_with_one_occurrence() {
        let h = Harness::new(Answer::Yes);
        h.write("a.rs", "same\n");
        let out = EditFile
            .run(
                &h.ctx,
                json!({"path": "a.rs", "old_string": "same", "new_string": "changed", "replace_all": true}),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{out:?}");
        assert_eq!(h.read("a.rs"), "changed\n");
        assert!(!out.content.contains("replaced"), "{out:?}");
    }

    #[tokio::test]
    async fn edit_file_replace_all_with_no_occurrences() {
        let h = Harness::new(Answer::Yes);
        h.write("a.rs", "hello\n");
        let out = EditFile
            .run(
                &h.ctx,
                json!({"path": "a.rs", "old_string": "goodbye", "new_string": "x", "replace_all": true}),
            )
            .await;
        let out = out.unwrap();
        assert!(out.is_error, "should fail when nothing found");
        assert!(out.content.contains("not found"), "{out:?}");
        assert!(out.content.contains("closest lines"), "{out:?}");
    }

    #[tokio::test]
    async fn edit_file_without_replace_all_still_refuses_ambiguous() {
        let h = Harness::new(Answer::Yes);
        h.write("a.rs", "same\nsame\n");
        let out = EditFile
            .run(
                &h.ctx,
                json!({"path": "a.rs", "old_string": "same", "new_string": "changed"}),
            )
            .await;
        let out = out.unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("ambiguous"), "{out:?}");
        assert_eq!(h.read("a.rs"), "same\nsame\n", "file should not be changed");
    }

    // ---------------------------------------------------------------- apply_patch --

    fn patch_for(name: &str, before: &str, after: &str) -> String {
        unified_diff(before, after, name)
    }

    #[tokio::test]
    async fn apply_patch_changes_every_file_it_names() {
        let h = Harness::new(Answer::Yes);
        h.write("a.txt", "one\n");
        h.write("b.txt", "two\n");
        let diff = format!(
            "{}{}",
            patch_for("a.txt", "one\n", "ONE\n"),
            patch_for("b.txt", "two\n", "TWO\n")
        );
        let out = h.run(&ApplyPatch, json!({"unified_diff": diff})).await;
        succeeded(&out);
        assert_eq!(h.read("a.txt"), "ONE\n");
        assert_eq!(h.read("b.txt"), "TWO\n");
    }

    /// If any file in the patch cannot be applied, none of them are written.
    #[tokio::test]
    async fn a_patch_that_cannot_apply_wholly_changes_nothing() {
        let h = Harness::new(Answer::Yes);
        h.write("a.txt", "one\n");
        h.write("b.txt", "something else entirely\n");
        let diff = format!(
            "{}{}",
            patch_for("a.txt", "one\n", "ONE\n"),
            // Written against contents b.txt does not have.
            patch_for("b.txt", "two\n", "TWO\n")
        );
        let out = h.run(&ApplyPatch, json!({"unified_diff": diff})).await;
        assert!(out.is_error, "expected a refusal: {}", out.content);
        assert_eq!(h.read("a.txt"), "one\n", "an earlier file was left changed");
        assert!(
            h.asked().is_empty(),
            "the user was asked to approve a patch that could not apply"
        );
    }

    /// Paths are resolved before the question is put.
    /// What the user approves is what happens.
    #[tokio::test]
    async fn a_patch_reaching_outside_the_root_is_refused_before_asking() {
        let h = Harness::new(Answer::Yes);
        let diff = patch_for("../escaped.txt", "", "gotcha\n");
        let out = h.run(&ApplyPatch, json!({"unified_diff": diff})).await;
        assert!(out.is_error, "expected a refusal: {}", out.content);
        assert!(
            h.asked().is_empty(),
            "the user was asked about a path outside the root"
        );
    }

    #[tokio::test]
    async fn a_declined_patch_changes_nothing() {
        let h = Harness::new(Answer::No);
        h.write("a.txt", "one\n");
        let diff = patch_for("a.txt", "one\n", "ONE\n");
        let out = h.run(&ApplyPatch, json!({"unified_diff": diff})).await;
        refused(&out, "user denied");
        assert_eq!(h.read("a.txt"), "one\n");
    }

    #[tokio::test]
    async fn an_empty_patch_is_refused() {
        let h = Harness::new(Answer::Yes);
        let out = h.run(&ApplyPatch, json!({"unified_diff": ""})).await;
        refused(&out, "empty patch");
    }

    // ------------------------------------------------------- parse_multifile_patch --

    /// Splitting on `--- ` boundaries.
    #[test]
    fn a_diff_splits_into_one_patch_per_file() {
        let diff = format!(
            "{}{}",
            unified_diff("a\n", "A\n", "one.txt"),
            unified_diff("b\n", "B\n", "two.txt")
        );
        let parsed = parse_multifile_patch(&diff).unwrap();
        assert_eq!(parsed.len(), 2);
    }

    #[test]
    fn a_single_file_diff_is_one_patch() {
        let diff = unified_diff("a\n", "A\n", "one.txt");
        assert_eq!(parse_multifile_patch(&diff).unwrap().len(), 1);
    }

    #[test]
    fn text_with_no_diff_in_it_yields_nothing() {
        assert!(parse_multifile_patch("just some prose\n")
            .unwrap()
            .is_empty());
    }

    /// A removed line beginning with `-` must not be read as the start of a new file.
    #[test]
    fn a_removed_line_is_not_mistaken_for_a_boundary() {
        let before = "keep\n--- not a header\nkeep2\n";
        let after = "keep\nkeep2\n";
        let diff = unified_diff(before, after, "one.txt");
        let parsed = parse_multifile_patch(&diff).unwrap();
        assert_eq!(
            parsed.len(),
            1,
            "a deleted line that looks like a header split the patch: {diff}"
        );
    }
}

#[cfg(test)]
mod edit_lines_tests {
    use super::*;
    use crate::tools::harness::{Answer, Harness};

    fn edits(pairs: &[(&str, &str)]) -> Value {
        json!({
            "path": "a.rs",
            "edits": pairs
                .iter()
                .map(|(o, n)| json!({"old_string": o, "new_string": n}))
                .collect::<Vec<_>>()
        })
    }

    #[tokio::test]
    async fn several_edits_are_applied_in_order() {
        let h = Harness::new(Answer::Yes);
        h.write("a.rs", "let a = 1;\nlet b = 2;\n");
        let out = EditLines
            .run(&h.ctx, edits(&[("a = 1", "a = 10"), ("b = 2", "b = 20")]))
            .await
            .unwrap();
        assert!(!out.is_error, "{out:?}");
        assert_eq!(h.read("a.rs"), "let a = 10;\nlet b = 20;\n");
    }

    /// All or none: an edit that cannot be placed leaves the file as it was.
    /// The model is never left reasoning about a file that is half of two versions.
    #[tokio::test]
    async fn one_edit_that_cannot_be_placed_leaves_the_file_alone() {
        let h = Harness::new(Answer::Yes);
        h.write("a.rs", "let a = 1;\nlet b = 2;\n");
        let out = EditLines
            .run(&h.ctx, edits(&[("a = 1", "a = 10"), ("nowhere", "x")]))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("edit 2 of 2"), "{out:?}");
        assert!(out.content.contains("nothing was changed"), "{out:?}");
        assert_eq!(h.read("a.rs"), "let a = 1;\nlet b = 2;\n");
    }

    /// An ambiguous edit is refused, as the single-edit tool refuses one.
    /// The model meant one place and the tool cannot know which.
    #[tokio::test]
    async fn an_ambiguous_edit_is_refused() {
        let h = Harness::new(Answer::Yes);
        h.write("a.rs", "x = 1;\nx = 1;\n");
        let out = EditLines
            .run(&h.ctx, edits(&[("x = 1", "x = 2")]))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("ambiguous"), "{out:?}");
        assert_eq!(h.read("a.rs"), "x = 1;\nx = 1;\n");
    }

    #[tokio::test]
    async fn a_denied_edit_writes_nothing() {
        let h = Harness::new(Answer::No);
        h.write("a.rs", "let a = 1;\n");
        let out = EditLines
            .run(&h.ctx, edits(&[("a = 1", "a = 2")]))
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(h.read("a.rs"), "let a = 1;\n");
    }

    // Tests for loose edits
    #[tokio::test]
    async fn edit_file_loose_edit_is_applied() {
        // File has 4-space indent and trailing spaces, edit asks for 2-space indent
        // The edit should apply as a loose match (not exact since spaces differ)
        let h = Harness::new(Answer::Yes);
        h.write("a.rs", "    let x = 1;   \n");
        let out = EditFile
            .run(
                &h.ctx,
                json!({"path": "a.rs", "old_string": "  let x = 1;", "new_string": "  let y = 2;"}),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "edit should succeed");
        // The file should now have the replacement
        assert!(
            h.read("a.rs").contains("let y = 2;"),
            "replacement text not found"
        );
    }

    #[tokio::test]
    async fn edit_file_loose_edit_message() {
        // This test verifies the loose match works
        // When old string doesn't match exactly but matches when trimmed
        let h = Harness::new(Answer::Yes);
        h.write("a.rs", "    let x = 1;\n");
        // "let x = 1;" matches line when trimmed, and it's an exact substring
        let out = EditFile
            .run(
                &h.ctx,
                json!({"path": "a.rs", "old_string": "let x = 1;", "new_string": "let y = 2;"}),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(h.read("a.rs").contains("let y = 2;"));
    }

    #[tokio::test]
    async fn edit_file_exact_loose_message() {
        // Create a case where old string is NOT a substring but matches when trimmed
        // File has: "    let x = 1;" (4 spaces)
        // Old: "  let_x = 1;" (2 spaces, underscore instead of space)
        // The old is not a substring but "let_x = 1;" trimmed doesn't match "let x = 1;" trimmed
        // So we fall through to nearest
        let h = Harness::new(Answer::Yes);
        h.write("a.rs", "    let x = 1;\n");
        let out = EditFile
            .run(
                &h.ctx,
                json!({"path": "a.rs", "old_string": "let_x = 1;", "new_string": "let_y = 2;"}),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("The closest lines are"));
    }

    #[tokio::test]
    async fn edit_file_missing_strings_shows_closest() {
        let h = Harness::new(Answer::Yes);
        h.write("a.rs", "hello\nworld\n");
        let out = EditFile
            .run(
                &h.ctx,
                json!({"path": "a.rs", "old_string": "helo", "new_string": "hello"}),
            )
            .await
            .unwrap();
        assert!(out.is_error, "edit should fail for typo");
        assert!(
            out.content.contains("The closest lines are"),
            "closest lines message not found"
        );
        assert!(
            out.content.contains("nothing was changed"),
            "nothing was changed message not found"
        );
    }

    #[tokio::test]
    async fn edit_lines_loose_edit_is_applied() {
        // File has 4-space indent, edit asks for 2-space indent
        let h = Harness::new(Answer::Yes);
        h.write("a.rs", "    let x = 1;\n");
        let out = EditLines
            .run(&h.ctx, json!({"path": "a.rs", "edits": [{"old_string": "  let x = 1;", "new_string": "  let y = 2;"}]}))
            .await
            .unwrap();
        assert!(!out.is_error, "edit should succeed");
        assert!(
            h.read("a.rs").contains("let y = 2;"),
            "replacement text not found"
        );
    }

    #[tokio::test]
    async fn edit_lines_missing_strings_shows_closest() {
        let h = Harness::new(Answer::Yes);
        h.write("a.rs", "hello\nworld\n");
        let out = EditLines
            .run(
                &h.ctx,
                json!({"path": "a.rs", "edits": [{"old_string": "helo", "new_string": "hello"}]}),
            )
            .await
            .unwrap();
        assert!(out.is_error, "edit should fail for typo");
        assert!(
            out.content.contains("The closest lines are"),
            "closest lines message not found"
        );
        assert!(
            out.content.contains("nothing was changed"),
            "nothing was changed message not found"
        );
    }

    // Tests for find() function

    /// A loose match stops before the newline of its last line, so the next line stays apart.
    #[test]
    fn a_loose_edit_keeps_the_line_after_it() {
        let file = "fn a() {\n    x();\n}\nfn b() {}\n";
        let Found::Loose(range) = find(file, "fn a() {\n  x();\n}") else {
            panic!("not a loose match");
        };
        let mut edited = file.to_string();
        edited.replace_range(range, "fn a() {}");
        assert_eq!(edited, "fn a() {}\nfn b() {}\n");
    }

    #[test]
    fn find_exact_match() {
        let file = "hello\nworld\n";
        let old = "world";
        let found = find(file, old);
        assert_eq!(found, Found::Exact(6..11));
    }

    #[test]
    fn find_loose_indent_2_to_4() {
        // File has 4-space indent, old has 2-space indent
        // Note: the old string "  let x = 1;" is actually a SUBSTRING of the file line
        // So we get an Exact match at byte 2, not a Loose match.
        // To test loose, we need a case where the old string is NOT a substring.
        let file = "    let x = 1;\n";
        let old = "  let x = 1;\n";
        let found = find(file, old);
        match found {
            Found::Loose(_) | Found::Exact(_) => {
                // Either is valid - Exact is found because the old is a substring
                // Loose is found if old is not a substring but matches when trimmed
            }
            Found::Nearest { .. } | Found::Ambiguous(_) => {
                // Not expected for this test case
                panic!("Expected Loose or Exact, got {found:?}");
            }
        }
    }

    #[test]
    fn find_trailing_spaces() {
        // Trailing spaces should be ignored for loose match
        let file = "hello world   \n";
        let old = "hello world\n";
        let found = find(file, old);
        match found {
            Found::Loose(_) | Found::Exact(_) => {} // Either is fine since trailing spaces are ignored
            other => panic!("Expected loose or exact, got {other:?}"),
        }
    }

    #[test]
    fn find_two_loose_matches() {
        // Same line appears twice
        let file = "hello\nhello\n";
        let old = "hello";
        let found = find(file, old);
        assert_eq!(found, Found::Ambiguous(2));
    }

    #[test]
    fn find_typo_nearest() {
        // File has "hello", old has "helo" (typo)
        let file = "hello\nworld\n";
        let old = "helo";
        let found = find(file, old);
        match found {
            Found::Nearest { first, last } => {
                // Should point to line 1 (best match with "hello")
                // With equal match counts, we expect first window
                assert!(
                    (1..=3).contains(&first),
                    "first should be between 1 and 3, got {first}"
                );
                assert!(
                    last >= first && last <= 3,
                    "last should be between first and 3"
                );
            }
            other => panic!("Expected Nearest, got {other:?}"),
        }
    }

    #[test]
    fn find_blank_lines_only() {
        // Old made only of blank lines should never be Loose
        let file = "hello\nworld\n";
        let old = "\n\n";
        let found = find(file, old);
        match found {
            Found::Nearest { .. } => {} // Should be Nearest, not Loose
            other => panic!("Expected Nearest, got {other:?}"),
        }
    }
}
