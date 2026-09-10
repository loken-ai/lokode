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
use crate::tools::text;

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
    fn name(&self) -> &str {
        "write_file"
    }
    fn mutates(&self) -> bool {
        true
    }
    fn description(&self) -> &str {
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
            Err(e) => return Ok(ToolOutput::err(allowlist::refusal(&ctx.root, &a.path, &e))),
        };

        let before = match text::read_text_if_exists(&target).await {
            Ok(b) => b.unwrap_or_default(),
            Err(e) => return Ok(ToolOutput::err(e)),
        };
        // A whole-file write replaces what the model believes the file says. If someone
        // has changed it since the model read it, that belief is stale and the write
        // throws their work away without either of them seeing it happen.
        if ctx.changed_since_seen(&target, before.as_bytes()) {
            return Ok(ToolOutput::err(format!(
                "{} changed on disk since you read it; read it again before writing it",
                ctx.short(&target)
            )));
        }
        let diff = unified_diff(&before, &a.content, &ctx.short(&target));

        match ctx
            .approve(ApprovalRequest {
                tool: self.name().into(),
                summary: format!(
                    "write_file {}  ({} → {} bytes){}",
                    ctx.short(&target),
                    before.len(),
                    a.content.len(),
                    losses(&diff)
                ),
                diff_preview: Some(diff.clone()),
                scope: write_scope(self.name(), &target),
            })
            .await
        {
            Approval::Proceed => {}
            Approval::Denied => return Ok(ToolOutput::err("user denied")),
            Approval::Interrupted => return Ok(ToolOutput::err("interrupted by user")),
        }

        write_atomic(&target, &a.content).await?;
        ctx.note_content(&target, a.content.as_bytes());

        Ok(ToolOutput::ok(format!(
            "wrote {} ({} bytes)\n{}",
            ctx.short(&target),
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
    fn name(&self) -> &str {
        "edit_file"
    }
    fn mutates(&self) -> bool {
        true
    }
    fn description(&self) -> &str {
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
            Err(e) => return Ok(ToolOutput::err(allowlist::refusal(&ctx.root, &a.path, &e))),
        };
        let before = match text::read_text(&target).await {
            Ok(b) => b,
            Err(e) => return Ok(ToolOutput::err(e)),
        };
        let count = before.matches(&a.old_string).count();
        if count == 0 {
            return Ok(ToolOutput::err(format!(
                "old_string not found in {}",
                ctx.short(&target)
            )));
        }
        if count > 1 {
            return Ok(ToolOutput::err(format!(
                "old_string is ambiguous: found {count} matches in {}; add surrounding context to make it unique",
                ctx.short(&target)
            )));
        }

        let after = before.replacen(&a.old_string, &a.new_string, 1);
        let diff = unified_diff(&before, &after, &ctx.short(&target));

        match ctx
            .approve(ApprovalRequest {
                tool: self.name().into(),
                summary: format!("edit_file {}", ctx.short(&target)),
                diff_preview: Some(diff.clone()),
                scope: write_scope(self.name(), &target),
            })
            .await
        {
            Approval::Proceed => {}
            Approval::Denied => return Ok(ToolOutput::err("user denied")),
            Approval::Interrupted => return Ok(ToolOutput::err("interrupted by user")),
        }

        write_atomic(&target, &after).await?;
        ctx.note_content(&target, after.as_bytes());
        Ok(ToolOutput::ok(format!(
            "edited {}\n{}",
            ctx.short(&target),
            preview(&diff)
        )))
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
    /// Existing substring to replace. Must occur exactly once in the file as it stands
    /// when this edit's turn comes.
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
    /// Applied to the text in memory first, so an edit that cannot be placed leaves the
    /// file as it was rather than half-changed. Each is checked against the text as the
    /// earlier ones left it, which is what the model means when it writes them in order.
    async fn run(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput> {
        let a: EditLinesArgs = serde_json::from_value(args)?;
        if a.edits.is_empty() {
            return Ok(ToolOutput::err("no edits given"));
        }
        let target = match allowlist::resolve(&ctx.root, &a.path, true) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::err(allowlist::refusal(&ctx.root, &a.path, &e))),
        };
        let before = match text::read_text(&target).await {
            Ok(b) => b,
            Err(e) => return Ok(ToolOutput::err(e)),
        };

        let mut after = before.clone();
        for (i, edit) in a.edits.iter().enumerate() {
            let count = after.matches(&edit.old_string).count();
            if count != 1 {
                return Ok(ToolOutput::err(format!(
                    "edit {} of {}: old_string {} in {}; nothing was changed",
                    i + 1,
                    a.edits.len(),
                    if count == 0 {
                        "was not found".to_string()
                    } else {
                        format!("matched {count} times, so it is ambiguous")
                    },
                    ctx.short(&target)
                )));
            }
            after = after.replacen(&edit.old_string, &edit.new_string, 1);
        }
        let diff = unified_diff(&before, &after, &ctx.short(&target));

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
            Approval::Denied => return Ok(ToolOutput::err("user denied")),
            Approval::Interrupted => return Ok(ToolOutput::err("interrupted by user")),
        }

        write_atomic(&target, &after).await?;
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
    /// Unified diff. File paths in the diff are interpreted relative to the project root.
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

        // Everything is resolved and applied in memory before anything is asked or
        // written. Resolving afterwards, as this did, meant the user approved a diff
        // whose paths had not been checked yet, and rejections were folded into the
        // summary - so what was agreed to and what happened could differ.
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
            let before = match tokio::fs::read_to_string(&target).await {
                Ok(s) => Some(s),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
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
            Approval::Denied => return Ok(ToolOutput::err("user denied")),
            Approval::Interrupted => return Ok(ToolOutput::err("interrupted by user")),
        }

        // A patch is one change to a project, not a sequence of unrelated ones. Stopping
        // half way through used to leave the earlier files modified and report it in a
        // line of prose, which is a tree in a state nobody asked for.
        let mut committed: Vec<&Staged> = Vec::with_capacity(staged.len());
        for s in &staged {
            if let Err(e) = write_atomic(&s.target, &s.after).await {
                let rolled = roll_back(&committed).await;
                return Ok(ToolOutput::err(format!(
                    "{}: write failed: {e}\n{rolled}",
                    ctx.short(&s.target)
                )));
            }
            committed.push(s);
        }
        // Once the whole patch is on disk: a run that rolled back left the files as they
        // were, and what was seen of them is still what is there.
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
/// Reported rather than returned: the write that failed is the error the caller cares
/// about, and whether the undo itself succeeded is something the user has to be told
/// either way.
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
/// A plain write truncates and then fills, so anything that interrupts it - a crash, a
/// full disk, the user quitting - leaves a file that is neither the old version nor the
/// new one. Writing beside the target and renaming over it means a reader sees one or the
/// other. The temporary lives in the same directory so the rename stays on one
/// filesystem, where it is atomic.
async fn write_atomic(target: &std::path::Path, contents: &str) -> Result<()> {
    let parent = target
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", target.display()))?;
    tokio::fs::create_dir_all(parent).await?;

    let tmp = parent.join(format!(".verve-{}.tmp", uuid::Uuid::new_v4()));
    let write = async {
        let mut f = tokio::fs::File::create(&tmp).await?;
        f.write_all(contents.as_bytes()).await?;
        // Durable before the rename publishes it, or a crash can leave the name pointing
        // at a file whose contents never reached the disk.
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
/// A write that replaces a file is the one change that can quietly lose work: the model
/// reproduces what it remembers of the file and drops the rest. The line counts are in
/// the diff already, but the diff is long and the summary is what is read.
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
/// (with a `+++` companion). diffy expects one patch per parse call.
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

    /// A file somebody changed while the model was thinking is not the file the model
    /// read. Overwriting it whole would throw their work away with nothing to show it.
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

    /// Reading it again is what makes the write allowed: the model is now writing over
    /// what is actually there.
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
            .filter(|n| n.starts_with(".verve-"))
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

    /// A patch is one change to a project. If a later file cannot be written, the earlier
    /// ones must not be left changed - that is a tree in a state nobody asked for.
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

    /// The scope an `Always` on a multi-file patch grants must cover every file it was
    /// shown, and nothing above them.
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

    /// The scope shown to the user is the one that gets recorded, and it is the directory
    /// the file lives in - not the tool.
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

    /// The whole point of staging: if any file in the patch cannot be applied, none of
    /// them are written.
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

    /// Paths are resolved before the question is put, so what the user approves is what
    /// happens.
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

    /// Hand-rolled splitting on `--- ` boundaries, and until now untested.
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

    /// All or none: an edit that cannot be placed leaves the file as it was, so the model
    /// is never left reasoning about a file that is half of two versions.
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

    /// An ambiguous edit is refused for the same reason the single-edit tool refuses one:
    /// the model meant one place and the tool cannot know which.
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
}
