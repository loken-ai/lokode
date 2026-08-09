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
    fn name(&self) -> &'static str {
        "write_file"
    }
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

        let before = match text::read_text_if_exists(&target).await {
            Ok(b) => b.unwrap_or_default(),
            Err(e) => return Ok(ToolOutput::err(e)),
        };
        let diff = unified_diff(&before, &a.content, &target.display().to_string());

        match ctx
            .approve(ApprovalRequest {
                tool: self.name().into(),
                summary: format!(
                    "write_file {}  ({} → {} bytes)",
                    target.display(),
                    before.len(),
                    a.content.len()
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
    fn name(&self) -> &'static str {
        "edit_file"
    }
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
        let before = match text::read_text(&target).await {
            Ok(b) => b,
            Err(e) => return Ok(ToolOutput::err(e)),
        };
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

        match ctx
            .approve(ApprovalRequest {
                tool: self.name().into(),
                summary: format!("edit_file {}", target.display()),
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
    fn name(&self) -> &'static str {
        "apply_patch"
    }
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
                Err(e) => return Ok(ToolOutput::err(format!("{rel}: {e}"))),
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
                        .map(|s| format!("  {}", s.target.display()))
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
                    s.target.display()
                )));
            }
            committed.push(s);
        }

        Ok(ToolOutput::ok(
            staged
                .iter()
                .map(|s| format!("patched {}", s.target.display()))
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
}
