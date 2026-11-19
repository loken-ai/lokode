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
        "Read a text file. Optional 1-indexed inclusive start_line/end_line. Without them, a \
         long file comes back as its first and last lines with its line count."
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
            Err(e) => return Ok(ToolOutput::err(allowlist::refusal(&ctx.root, &a.path, &e))),
        };
        // What the editor holds comes first.
        // A buffer not saved yet is what the person is looking at.
        let held = ctx.editor_text(&resolved).await;
        let body = match &held {
            Some(t) => text::number_window(t, a.start_line, a.end_line),
            None => match text::read_numbered(&resolved, a.start_line, a.end_line).await {
                Ok(b) => b,
                Err(e) => return Ok(ToolOutput::err(e)),
            },
        };
        // What the file held when it was read.
        // A later whole-file write can tell whether it is replacing what the model saw or somebody
        // else's work.
        match held {
            Some(t) => ctx.note_content(&resolved, t.as_bytes()),
            None => {
                if let Ok(bytes) = tokio::fs::read(&resolved).await {
                    ctx.note_content(&resolved, &bytes);
                }
            }
        }

        // A read without a range shows the file's ends when the whole would not fit the step's
        // share of the window.
        let body = if a.start_line.is_none() && a.end_line.is_none() {
            text::ends_of(body, ctx.whole_read_chars)
        } else {
            body
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
            Err(e) => return Ok(ToolOutput::err(allowlist::refusal(&ctx.root, &a.path, &e))),
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
        // Clamped like every other result.
        // One result must not be able to displace the conversation.
        Ok(ToolOutput::ok(ctx.clamp(entries.join("\n"))))
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

        Ok(ToolOutput::ok(ctx.clamp(hits.join("\n"))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// A path that is wrong about where a file sits, but right about the file, is answered
    /// with where it is.
    #[tokio::test]
    async fn a_path_that_is_not_there_says_where_the_file_is() {
        let h = Harness::new(Answer::Yes);
        h.write("src/brew.rs", "fn strength() {}");
        let out = ReadFile
            .run(&h.ctx, serde_json::json!({"path": "workspace/src/brew.rs"}))
            .await
            .unwrap();
        assert!(out.is_error, "{out:?}");
        assert!(out.content.contains("src/brew.rs"), "{}", out.content);
        assert!(out.content.contains("there is one at"), "{}", out.content);
    }

    /// A read without a range of a file longer than the step's share shows the file's ends.
    /// A read with a range returns the range whole.
    #[tokio::test]
    async fn a_whole_read_of_a_long_file_shows_its_ends() {
        let mut h = Harness::new(Answer::Yes);
        let text: String = (1..=500)
            .map(|n| format!("\"field_{n}\": {n},\n"))
            .collect();
        h.write("record.json", &text);
        h.ctx.whole_read_chars = 2000;
        let whole = ReadFile
            .run(&h.ctx, serde_json::json!({"path": "record.json"}))
            .await
            .unwrap();
        assert!(!whole.is_error, "{whole:?}");
        assert!(whole.content.len() <= 2200, "{}", whole.content.len());
        assert!(
            whole.content.contains("of 500 left out"),
            "{}",
            whole.content
        );
        assert!(
            whole.content.contains("\"field_1\": 1,"),
            "{}",
            whole.content
        );
        assert!(
            whole.content.contains("\"field_500\": 500,"),
            "{}",
            whole.content
        );
        let range = ReadFile
            .run(
                &h.ctx,
                serde_json::json!({"path": "record.json", "start_line": 200, "end_line": 300}),
            )
            .await
            .unwrap();
        assert_eq!(range.content.lines().count(), 101, "{}", range.content);
    }

    /// A name nothing in the project has gets the refusal alone.
    /// A suggestion that names nothing is noise.
    #[tokio::test]
    async fn a_name_the_project_does_not_have_gets_no_suggestion() {
        let h = Harness::new(Answer::Yes);
        h.write("src/brew.rs", "fn strength() {}");
        let out = ReadFile
            .run(&h.ctx, serde_json::json!({"path": "src/kettle.rs"}))
            .await
            .unwrap();
        assert!(out.is_error, "{out:?}");
        assert!(!out.content.contains("there is one at"), "{}", out.content);
    }

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

    /// Reading is not gated on approval.
    /// The root is the only thing standing between the model and the rest of the filesystem.
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

    /// The walk is gitignore-aware.
    /// That is the difference between listing a project and listing its build directory.
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

    /// Outside a repository a `.gitignore` carries no authority.
    /// The walk returns the files anyway.
    /// The tool describes itself as gitignore-aware, and this is the edge of what that means.
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
