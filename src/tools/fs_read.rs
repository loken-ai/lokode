use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::sandbox::allowlist;
use crate::tools::registry::{Tool, ToolCtx, ToolOutput};
use crate::tools::text;

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
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
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
        let body = match text::read_numbered(&resolved, a.start_line, a.end_line).await {
            Ok(b) => b,
            Err(e) => return Ok(ToolOutput::err(e)),
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
    fn name(&self) -> &str {
        "list_dir"
    }
    fn description(&self) -> &str {
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
    fn name(&self) -> &str {
        "glob"
    }
    fn description(&self) -> &str {
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
                let entry = match r {
                    Ok(e) => e,
                    Err(_) => continue,
                };
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::harness::{outside, refused, succeeded, Answer, Harness};
    use serde_json::json;

    // --------------------------------------------------------------- read_file --

    #[tokio::test]
    async fn read_file_numbers_the_lines_it_returns() {
        let h = Harness::new(Answer::No);
        h.write("a.txt", "alpha\nbeta\ngamma\n");
        let out = h.run(&ReadFile, json!({"path": "a.txt"})).await;
        succeeded(&out);
        assert_eq!(out.content, "    1 | alpha\n    2 | beta\n    3 | gamma");
    }

    #[tokio::test]
    async fn read_file_honours_a_line_window() {
        let h = Harness::new(Answer::No);
        h.write("a.txt", "one\ntwo\nthree\nfour\n");
        let out = h
            .run(
                &ReadFile,
                json!({"path": "a.txt", "start_line": 2, "end_line": 3}),
            )
            .await;
        succeeded(&out);
        assert_eq!(out.content, "    2 | two\n    3 | three");
    }

    /// Reading is not gated on approval, so the root is the only thing standing between
    /// the model and the rest of the filesystem.
    #[tokio::test]
    async fn read_file_refuses_to_leave_the_root() {
        let h = Harness::new(Answer::No);
        let out = h
            .run(&ReadFile, json!({"path": outside().to_str().unwrap()}))
            .await;
        refused(&out, "outside the project root");
    }

    #[tokio::test]
    async fn read_file_refuses_a_binary_file() {
        let h = Harness::new(Answer::No);
        h.write("logo.png", "\u{0}\u{0}not text");
        let out = h.run(&ReadFile, json!({"path": "logo.png"})).await;
        refused(&out, "not a text file");
    }

    #[tokio::test]
    async fn read_file_reports_a_missing_file_rather_than_failing() {
        let h = Harness::new(Answer::No);
        let out = h.run(&ReadFile, json!({"path": "nope.txt"})).await;
        assert!(out.is_error, "expected a refusal: {}", out.content);
    }

    // ---------------------------------------------------------------- list_dir --

    #[tokio::test]
    async fn list_dir_shows_children_with_their_kind() {
        let h = Harness::new(Answer::No);
        h.write("src/main.rs", "fn main() {}");
        h.write("README.md", "hi");
        let out = h.run(&ListDir, json!({"path": "."})).await;
        succeeded(&out);
        assert!(out.content.contains("README.md"), "{}", out.content);
        assert!(
            out.content.contains("DIR"),
            "src should be listed as a directory: {}",
            out.content
        );
        // Immediate children only.
        assert!(!out.content.contains("main.rs"), "{}", out.content);
    }

    #[tokio::test]
    async fn list_dir_refuses_to_leave_the_root() {
        let h = Harness::new(Answer::No);
        let out = h.run(&ListDir, json!({"path": "/etc"})).await;
        refused(&out, "outside the project root");
    }

    // -------------------------------------------------------------------- glob --

    #[tokio::test]
    async fn glob_finds_files_recursively() {
        let h = Harness::new(Answer::No);
        h.write("src/a.rs", "");
        h.write("src/deep/b.rs", "");
        h.write("notes.md", "");
        let out = h.run(&Glob, json!({"pattern": "**/*.rs"})).await;
        succeeded(&out);
        assert!(out.content.contains("src/a.rs"), "{}", out.content);
        assert!(out.content.contains("src/deep/b.rs"), "{}", out.content);
        assert!(!out.content.contains("notes.md"), "{}", out.content);
    }

    /// The walk is gitignore-aware, which is the difference between listing a project and
    /// listing its build directory.
    #[tokio::test]
    async fn glob_skips_what_the_project_ignores() {
        let h = Harness::new(Answer::No);
        // The rules only apply inside a repository, so there has to be one.
        std::fs::create_dir_all(h.path(".git")).unwrap();
        h.write(".gitignore", "target/\n");
        h.write("src/a.rs", "");
        h.write("target/debug/b.rs", "");
        let out = h.run(&Glob, json!({"pattern": "**/*.rs"})).await;
        succeeded(&out);
        assert!(out.content.contains("src/a.rs"), "{}", out.content);
        assert!(
            !out.content.contains("target/debug/b.rs"),
            "an ignored path was walked: {}",
            out.content
        );
    }

    /// Outside a repository a `.gitignore` carries no authority, and the walk says so by
    /// returning the files anyway. Worth pinning down: the tool describes itself as
    /// gitignore-aware, and this is the edge of what that means.
    #[tokio::test]
    async fn glob_ignores_a_gitignore_with_no_repository_around_it() {
        let h = Harness::new(Answer::No);
        h.write(".gitignore", "target/\n");
        h.write("target/debug/b.rs", "");
        let out = h.run(&Glob, json!({"pattern": "**/*.rs"})).await;
        succeeded(&out);
        assert!(out.content.contains("target/debug/b.rs"), "{}", out.content);
    }

    #[tokio::test]
    async fn glob_matching_nothing_is_not_an_error() {
        let h = Harness::new(Answer::No);
        h.write("a.txt", "");
        let out = h.run(&Glob, json!({"pattern": "**/*.rs"})).await;
        succeeded(&out);
        assert_eq!(out.content, "");
    }
}
