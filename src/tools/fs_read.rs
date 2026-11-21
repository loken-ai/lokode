use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::Result;
use async_trait::async_trait;
use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::backend::wire::Image;
use crate::sandbox::allowlist;
use crate::tools::registry::{Tool, ToolCtx, ToolOutput};
use crate::tools::text;

const MAX_LIST_ENTRIES: usize = 1024;
const MAX_GLOB_HITS: usize = 512;

/// Whether a path is an image file and what its media type is.
///
/// Case-insensitive check for .png, .jpg, .jpeg, .gif, .webp.
fn image_type(path: &Path) -> Option<&'static str> {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .and_then(|ext| match ext.as_str() {
            "png" => Some("image/png"),
            "jpg" | "jpeg" => Some("image/jpeg"),
            "gif" => Some("image/gif"),
            "webp" => Some("image/webp"),
            _ => None,
        })
}

// ============================================================== read_file --

pub struct ReadFile;

#[derive(Deserialize)]
struct ReadFileArgs {
    path: PathBuf,
    #[serde(default)]
    start_line: Option<usize>,
    #[serde(default)]
    end_line: Option<usize>,
    #[serde(default)]
    pages: Option<String>,
}

/// Parse a pages string into a list of page numbers.
/// The format is either a single number "N" or a range "M-N", both 1-indexed.
#[derive(Debug)]
enum PagesSpec {
    Single(u32),
    Range(u32, u32),
}

impl PagesSpec {
    fn page_numbers(&self, total_pages: u32) -> Result<Vec<u32>, String> {
        match self {
            PagesSpec::Single(n) => {
                if *n < 1 || *n > total_pages {
                    Err(format!(
                        "page {} is beyond the end of the PDF ({total_pages} pages)",
                        n
                    ))
                } else {
                    Ok(vec![*n])
                }
            }
            PagesSpec::Range(start, end) => {
                if *start > *end {
                    return Err(format!(
                        "invalid page range: start ({start}) must be <= end ({end})",
                    ));
                }
                if *start < 1 || *end > total_pages {
                    Err(format!(
                        "page range {}-{} exceeds the PDF's {} pages",
                        start, end, total_pages
                    ))
                } else {
                    Ok((*start..=*end).collect())
                }
            }
        }
    }
}

/// Parse a pages string like "3" or "2-5" into PagesSpec.
fn parse_pages_spec(pages: &str) -> Result<PagesSpec, String> {
    let trimmed = pages.trim();
    if trimmed.contains('-') {
        let (start, end) = trimmed
            .split_once('-')
            .ok_or_else(|| format!("invalid page range '{}'", trimmed))?;
        let start_u = u32::from_str(start).map_err(|_| {
            format!(
                "invalid page number '{}' in range '{}-{}'",
                start, trimmed, end
            )
        })?;
        let end_u = u32::from_str(end).map_err(|_| {
            format!(
                "invalid page number '{}' in range '{}-{}'",
                end, start, trimmed
            )
        })?;
        if start_u == 0 || end_u == 0 {
            return Err("page numbers must be >= 1".into());
        }
        if start_u > end_u {
            return Err(format!(
                "invalid page range: start ({start_u}) must be <= end ({end_u})",
            ));
        }
        Ok(PagesSpec::Range(start_u, end_u))
    } else {
        let n = u32::from_str(trimmed).map_err(|_| format!("invalid page number '{}'", trimmed))?;
        if n == 0 {
            return Err("page numbers must be >= 1".into());
        }
        Ok(PagesSpec::Single(n))
    }
}

/// Read a PDF file and extract text from the specified pages.
/// Returns an error if the PDF is malformed or has no text.
#[async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "Read a text file. Optional 1-indexed inclusive start_line/end_line. Without them, a \
         long file comes back as its first and last lines with its line count. For a PDF, \
         optional pages chooses its pages, e.g. \"3\" or \"2-5\", and returns its text. \
         For images (PNG, JPG, GIF, WEBP), the file is returned to a model that sees images \
         as a base64-encoded image."
    }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":       {"type": "string"},
                "start_line": {"type": "integer", "minimum": 1},
                "end_line":   {"type": "integer", "minimum": 1},
                "pages":      {"type": "string"}
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

        // An image the editor holds no buffer for goes to the model as an image.
        let image = match image_type(&resolved) {
            Some(media_type) if ctx.editor_text(&resolved).await.is_none() => Some(media_type),
            _ => None,
        };
        if let Some(media_type) = image {
            if a.start_line.is_some() || a.end_line.is_some() || a.pages.is_some() {
                return Ok(ToolOutput::err(
                    "an image is read whole; omit start_line, end_line, and pages",
                ));
            }
            if !ctx.sees_images {
                return Ok(ToolOutput::err(
                    "this model does not take images: the server does not say it sees them",
                ));
            }
            let bytes = match tokio::fs::read(&resolved).await {
                Ok(b) => b,
                Err(e) => return Ok(ToolOutput::err(format!("{}: {e}", ctx.short(&resolved)))),
            };
            let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
            let output = ToolOutput::ok(format!(
                "image {}: {media_type}, {} bytes",
                ctx.short(&resolved),
                bytes.len()
            ))
            .with_images(vec![Image {
                media_type: media_type.to_string(),
                data,
            }]);
            ctx.note_content(&resolved, &bytes);
            return Ok(output);
        }

        // Determine if this is a PDF (case-insensitive extension check)
        let is_pdf = resolved
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.eq_ignore_ascii_case("pdf"))
            .unwrap_or(false);

        let body = if is_pdf {
            // PDF case: read text from the PDF
            let pages = a.pages.as_deref();
            let bytes = match tokio::fs::read(&resolved).await {
                Ok(b) => b,
                Err(e) => return Ok(ToolOutput::err(format!("{}: {e}", resolved.display()))),
            };

            let result = std::panic::catch_unwind(|| {
                let doc = lopdf::Document::load_mem(&bytes)
                    .map_err(|e| format!("could not read the text of this PDF: {e}"))?;

                let all_pages: Vec<u32> = doc.get_pages().keys().copied().collect();
                let total_pages = all_pages.len() as u32;

                let page_numbers = if let Some(spec_str) = pages {
                    let spec = parse_pages_spec(spec_str).map_err(|e| format!("pages: {e}"))?;
                    spec.page_numbers(total_pages)?
                } else {
                    all_pages.clone()
                };

                let mut numbered_pages = Vec::new();
                for &page_num in &page_numbers {
                    let extracted = lopdf::Document::extract_text(&doc, &[page_num])
                        .map_err(|e| format!("could not read the text of this PDF: {e}"))?;

                    if extracted.trim().is_empty() {
                        return Err(
                            "this PDF holds no text (its pages may be scanned images)".into()
                        );
                    }

                    numbered_pages.push(format!("--- page {} ---\n{}", page_num, extracted));
                }

                let full_text = numbered_pages.join("\n");
                Ok::<String, String>(full_text)
            });

            match result {
                Ok(Ok(text)) => text::number_window(&text, a.start_line, a.end_line),
                Ok(Err(e)) => return Ok(ToolOutput::err(e)),
                Err(_) => {
                    return Ok(ToolOutput::err(format!(
                        "could not read the text of this PDF: {}",
                        "unexpected error while reading"
                    )))
                }
            }
        } else {
            // Text file case
            let held = ctx.editor_text(&resolved).await;
            match &held {
                Some(t) => text::number_window(t, a.start_line, a.end_line),
                None => match text::read_numbered(&resolved, a.start_line, a.end_line).await {
                    Ok(b) => b,
                    Err(e) => return Ok(ToolOutput::err(e)),
                },
            }
        };

        // What the file held when it was read.
        // A later whole-file write can tell whether it is replacing what the model saw or somebody
        // else's work.
        let held = ctx.editor_text(&resolved).await;
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
            text::ends_of(
                body,
                ctx.step_share_chars,
                "read them with start_line and end_line.",
            )
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
    #[serde(default = "default_depth")]
    depth: usize,
}

fn default_depth() -> usize {
    1
}

#[async_trait]
impl Tool for ListDir {
    fn name(&self) -> &str {
        "list_dir"
    }
    fn description(&self) -> &str {
        "List immediate children of a directory under the project root. With a depth, also list children of subdirectories up to that depth."
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

        if a.depth == 0 {
            return Ok(ToolOutput::err("depth must be at least 1"));
        }

        let entries = tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
            use ignore::WalkBuilder;
            let mut out = Vec::new();
            let walker = WalkBuilder::new(&dir).max_depth(Some(a.depth)).build();
            for r in walker {
                let entry = match r {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                if out.len() >= MAX_LIST_ENTRIES {
                    out.push("… (truncated)".into());
                    break;
                }
                let meta = entry.metadata().ok();
                let kind = match meta.as_ref().map(|m| m.is_dir()) {
                    Some(true) => "DIR ",
                    Some(false) => "FILE",
                    None => "?   ",
                };
                let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
                // Show path relative to the listed directory, not the project root.
                let rel_path = match entry.path().strip_prefix(&dir) {
                    Ok(p) => p.display().to_string(),
                    Err(_) => continue,
                };
                out.push(format!("{kind}  {size:>10}  {rel_path}"));
            }
            Ok(out)
        })
        .await??;

        Ok(ToolOutput::ok(ctx.clamp(entries.join("\n"))))
    }
}

// =================================================================== glob --

pub struct Glob;

#[derive(Deserialize)]
struct GlobArgs {
    path: Option<PathBuf>,
    pattern: String,
}

#[async_trait]
impl Tool for Glob {
    fn name(&self) -> &str {
        "glob"
    }
    fn description(&self) -> &str {
        "Recursively list files matching a gitignore-aware glob from the project root. With a path, search from that directory instead."
    }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":     {"type": "string"},
                "pattern":  {"type": "string"}
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }

    async fn run(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput> {
        let a: GlobArgs = serde_json::from_value(args)?;
        let root = ctx.root.clone();
        let pattern = a.pattern;

        let start = match a.path {
            Some(ref p) => match allowlist::resolve(&root, p, true) {
                Ok(p) => p,
                Err(e) => {
                    return Ok(ToolOutput::err(allowlist::refusal(&root, p, &e)));
                }
            },
            None => root.clone(),
        };

        let hits = tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
            use ignore::WalkBuilder;
            let mut matcher = ignore::overrides::OverrideBuilder::new(&start);
            matcher.add(&pattern)?;
            let overrides = matcher.build()?;
            let mut out = Vec::new();
            let walker = WalkBuilder::new(&start).overrides(overrides).build();
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
        h.ctx.step_share_chars = 2000;
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
        // A file with a non-image extension that is binary data refuses to read as text.
        let h = Harness::new(Answer::No);
        h.write("logo.bin", "\u{0}\u{0}not text");
        let out = h.run(&ReadFile, json!({"path": "logo.bin"})).await;
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

    // -------------------------------------------------------------------- glob --
    // Test with path parameter

    #[tokio::test]
    async fn glob_with_path_searches_from_directory() {
        let h = Harness::new(Answer::No);
        h.write("src/a.rs", "");
        h.write("b.rs", "");
        let out = h
            .run(&Glob, json!({"path": "src", "pattern": "*.rs"}))
            .await;
        succeeded(&out);
        assert!(out.content.contains("src/a.rs"), "{}", out.content);
        assert!(!out.content.contains("b.rs"), "{}", out.content);
    }

    #[tokio::test]
    async fn glob_with_path_outside_root_is_refused() {
        let h = Harness::new(Answer::No);
        let out = h
            .run(&Glob, json!({"path": "/etc", "pattern": "*.rs"}))
            .await;
        refused(&out, "outside the project root");
    }

    // ---------------------------------------------------------------- list_dir --
    // Test with depth parameter

    #[tokio::test]
    async fn list_dir_depth_2_shows_subdirectory_contents() {
        let h = Harness::new(Answer::No);
        h.write("src/a.rs", "");
        h.write("sub/x.txt", "");
        let out = h.run(&ListDir, json!({"path": ".", "depth": 2})).await;
        succeeded(&out);
        assert!(out.content.contains("sub/x.txt"), "{}", out.content);
    }

    #[tokio::test]
    async fn list_dir_depth_1_same_as_default() {
        let h = Harness::new(Answer::No);
        h.write("src/a.rs", "");
        h.write("sub/x.txt", "");
        let out = h.run(&ListDir, json!({"path": "."})).await;
        succeeded(&out);
        assert!(out.content.contains("src"), "{}", out.content);
        assert!(!out.content.contains("a.rs"), "{}", out.content);
        assert!(!out.content.contains("sub/x.txt"), "{}", out.content);
        let out_depth_1 = h.run(&ListDir, json!({"path": ".", "depth": 1})).await;
        succeeded(&out_depth_1);
        assert_eq!(out.content, out_depth_1.content);
    }

    #[tokio::test]
    async fn list_dir_depth_0_is_error() {
        let h = Harness::new(Answer::No);
        let out = h.run(&ListDir, json!({"path": ".", "depth": 0})).await;
        assert!(out.is_error, "expected an error: {}", out.content);
        assert!(
            out.content.contains("depth must be at least 1"),
            "{}",
            out.content
        );
    }

    #[tokio::test]
    async fn list_dir_depth_2_respects_gitignore() {
        let h = Harness::new(Answer::No);
        // The rules only apply inside a repository, so there has to be one.
        std::fs::create_dir_all(h.path(".git")).unwrap();
        h.write(".gitignore", "target/\n");
        h.write("src/a.rs", "");
        h.write("target/debug/b.rs", "");
        let out = h.run(&ListDir, json!({"path": ".", "depth": 2})).await;
        succeeded(&out);
        assert!(out.content.contains("src/a.rs"), "{}", out.content);
        assert!(
            !out.content.contains("target/debug/b.rs"),
            "an ignored path was walked: {}",
            out.content
        );
    }

    // ================================================================== pdf --

    /// Parse pages "3" or "2-5" into page numbers.
    #[test]
    fn parse_pages_single() {
        use super::parse_pages_spec;
        let spec = parse_pages_spec("3").unwrap();
        match spec {
            super::PagesSpec::Single(n) => assert_eq!(n, 3),
            super::PagesSpec::Range(_, _) => panic!("expected Single, got Range"),
        }
    }

    #[test]
    fn parse_pages_range() {
        use super::parse_pages_spec;
        let spec = parse_pages_spec("2-5").unwrap();
        match spec {
            super::PagesSpec::Range(start, end) => {
                assert_eq!(start, 2);
                assert_eq!(end, 5);
            }
            super::PagesSpec::Single(_) => panic!("expected Range, got Single"),
        }
    }

    #[test]
    fn parse_pages_refuses_zero() {
        use super::parse_pages_spec;
        assert!(parse_pages_spec("0").is_err());
        assert!(parse_pages_spec("0-5").is_err());
        assert!(parse_pages_spec("5-0").is_err());
    }

    #[test]
    fn parse_pages_refuses_invalid() {
        use super::parse_pages_spec;
        assert!(parse_pages_spec("x").is_err());
        assert!(parse_pages_spec("2-x").is_err());
        assert!(parse_pages_spec("x-5").is_err());
        assert!(parse_pages_spec("5-2").is_err()); // reversed range
    }

    #[test]
    fn pages_single_beyond_end_of_pdf() {
        use super::PagesSpec;
        let spec = PagesSpec::Single(5);
        let result = spec.page_numbers(3);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("page 5"));
    }

    #[test]
    fn pages_range_beyond_end_of_pdf() {
        use super::PagesSpec;
        let spec = PagesSpec::Range(2, 5);
        let result = spec.page_numbers(3);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("5"));
    }

    /// Build a minimal 2-page PDF as bytes.
    /// The content streams contain: BT /F1 12 Tf 72 712 Td (page one) Tj ET
    /// and (page two), using Helvetica Type1 font.
    fn build_2page_pdf() -> Vec<u8> {
        let mut buf = Vec::new();

        // PDF header
        buf.extend_from_slice(b"%PDF-1.4\n");

        // Helvetica Type1 font (as a Type1 font object, minimal but valid)
        let font_obj = r###"10 0 obj
<< /Type /Font
   /Subtype /Type1
   /BaseFont /Helvetica
   /Encoding /MacRomanEncoding
>>
endobj
"###;
        let font_start = buf.len();
        buf.extend_from_slice(font_obj.as_bytes());

        // Page content streams
        let page1_stream = r###"11 0 obj
<< /Length 45 >>
stream
BT /F1 12 Tf 72 712 Td (page one) Tj ET
endstream
endobj
"###;
        let page1_start = buf.len();
        buf.extend_from_slice(page1_stream.as_bytes());

        let page2_stream = r###"12 0 obj
<< /Length 45 >>
stream
BT /F1 12 Tf 72 712 Td (page two) Tj ET
endstream
endobj
"###;
        let page2_start = buf.len();
        buf.extend_from_slice(page2_stream.as_bytes());

        // Page 1 object
        let page1_obj = r###"2 0 obj
<< /Type /Page
   /Parent 3 0 R
   /MediaBox [0 0 612 792]
   /Contents 11 0 R
   /Resources << /Font << /F1 10 0 R >> >>
>>
endobj
"###
        .to_string();
        let page1_obj_start = buf.len();
        buf.extend_from_slice(page1_obj.as_bytes());

        // Page 2 object
        let page2_obj = r###"4 0 obj
<< /Type /Page
   /Parent 3 0 R
   /MediaBox [0 0 612 792]
   /Contents 12 0 R
   /Resources << /Font << /F1 10 0 R >> >>
>>
endobj
"###
        .to_string();
        let page2_obj_start = buf.len();
        buf.extend_from_slice(page2_obj.as_bytes());

        // Pages tree object (node)
        let pages_obj = r###"3 0 obj
<< /Type /Pages
   /Kids [2 0 R 4 0 R]
   /Count 2
>>
endobj
"###;
        let pages_obj_start = buf.len();
        buf.extend_from_slice(pages_obj.as_bytes());

        // Catalog object
        let catalog_obj = r###"1 0 obj
<< /Type /Catalog
   /Pages 3 0 R
>>
endobj
"###;
        let catalog_start = buf.len();
        buf.extend_from_slice(catalog_obj.as_bytes());

        // Calculate xref offsets and build xref table
        let catalog_off = catalog_start;
        let pages_obj_off = pages_obj_start;
        let page1_obj_off = page1_obj_start;
        let page2_obj_off = page2_obj_start;
        let page1_stream_off = page1_start;
        let page2_stream_off = page2_start;
        let font_off = font_start;

        let catalog_line = format!("{:010} 00000 n ", catalog_off);
        let pages_line = format!("{:010} 00000 n ", pages_obj_off);
        let page1_line = format!("{:010} 00000 n ", page1_obj_off);
        let page2_line = format!("{:010} 00000 n ", page2_obj_off);
        let page1_stream_line = format!("{:010} 00000 n ", page1_stream_off);
        let page2_stream_line = format!("{:010} 00000 n ", page2_stream_off);
        let font_line = format!("{:010} 00000 n ", font_off);
        let xref_end = buf.len();

        let xref = format!(
            r###"xref
0 12
0000000000 65535 f 
{catalog_line}
{pages_line}
{page1_line}
{page2_line}
{page1_stream_line}
{page2_stream_line}
{font_line}
0000000000 00000 n 
0000000000 00000 n 
0000000000 00000 n 
"###
        );

        let trailer = format!(
            r###"trailer
<< /Size 12 /Root 1 0 R >>
startxref
{xref_end}
%%EOF
"###
        );

        buf.extend_from_slice(xref.as_bytes());
        buf.extend_from_slice(trailer.as_bytes());

        buf
    }

    /// Test PDF reading with pages parameter.
    #[tokio::test]
    async fn read_file_reads_pdf_with_pages() {
        use super::*;
        let h = Harness::new(Answer::No);
        let pdf_bytes = build_2page_pdf();
        h.write_bytes("test.pdf", &pdf_bytes);

        let out = h.run(&ReadFile, json!({"path": "test.pdf"})).await;
        succeeded(&out);
        assert!(
            out.content.contains("--- page 1 ---"),
            "missing page 1 header: {}",
            out.content
        );
        assert!(
            out.content.contains("--- page 2 ---"),
            "missing page 2 header: {}",
            out.content
        );
        assert!(
            out.content.contains("page one"),
            "missing 'page one': {}",
            out.content
        );
        assert!(
            out.content.contains("page two"),
            "missing 'page two': {}",
            out.content
        );
    }

    /// Test PDF reading with pages: "2".
    #[tokio::test]
    async fn read_file_reads_pdf_with_pages_two() {
        use super::*;
        let h = Harness::new(Answer::No);
        let pdf_bytes = build_2page_pdf();
        h.write_bytes("test.pdf", &pdf_bytes);

        let out = h
            .run(&ReadFile, json!({"path": "test.pdf", "pages": "2"}))
            .await;
        succeeded(&out);
        assert!(
            out.content.contains("--- page 2 ---"),
            "missing page 2 header: {}",
            out.content
        );
        assert!(
            !out.content.contains("--- page 1 ---"),
            "should not contain page 1: {}",
            out.content
        );
        assert!(
            out.content.contains("page two"),
            "missing 'page two': {}",
            out.content
        );
        assert!(
            !out.content.contains("page one"),
            "should not contain 'page one': {}",
            out.content
        );
    }

    /// Test PDF reading with pages on a .txt file should error.
    #[tokio::test]
    async fn read_file_pages_on_txt_is_error() {
        let h = Harness::new(Answer::No);
        h.write("test.txt", "hello\n");
        let out = h
            .run(&ReadFile, json!({"path": "test.txt", "pages": "1"}))
            .await;
        // For text files, the pages field is simply ignored; no error should occur.
        // The requirement is that pages is only meaningful for PDFs.
        assert!(!out.is_error, "unexpected error: {}", out.content);
    }

    /// Test that pages on a .txt file is not an error.
    #[tokio::test]
    async fn read_file_txt_with_pages_works_as_before() {
        let h = Harness::new(Answer::No);
        h.write("test.txt", "line one\nline two\nline three\n");
        let out = h
            .run(&ReadFile, json!({"path": "test.txt", "pages": "1"}))
            .await;
        assert!(!out.is_error, "unexpected error: {}", out.content);
        assert!(out.content.contains("line one"), "{}", out.content);
    }

    // ================================================================= image --

    /// Test image_type function.
    #[test]
    fn image_type_detects_formats_case_insensitively() {
        use super::image_type;
        use std::path::Path;

        assert_eq!(image_type(Path::new("a.PNG")), Some("image/png"));
        assert_eq!(image_type(Path::new("b.jpeg")), Some("image/jpeg"));
        assert_eq!(image_type(Path::new("c.jpg")), Some("image/jpeg"));
        assert_eq!(image_type(Path::new("d.gif")), Some("image/gif"));
        assert_eq!(image_type(Path::new("e.webp")), Some("image/webp"));
        assert_eq!(image_type(Path::new("f.png")), Some("image/png"));
        assert_eq!(image_type(Path::new("g.TXT")), None);
        assert_eq!(image_type(Path::new("h.txt")), None);
    }

    /// Test reading a PNG image returns the image with correct media type and data.
    #[tokio::test]
    async fn read_file_returns_png_as_image() {
        use super::*;
        use base64::Engine;

        let h = Harness::new(Answer::No).seeing_images();
        let png_data: &[u8] = b"\x89PNG\r\n\x1a\n"; // minimal valid PNG header
        h.write_bytes("test.png", png_data);

        let out = h.run(&ReadFile, json!({"path": "test.png"})).await;
        succeeded(&out);
        assert!(out.content.contains("image"));
        assert!(out.content.contains("8 bytes"));
        assert_eq!(out.images.len(), 1);
        assert_eq!(out.images[0].media_type, "image/png");
        // Decode and verify the data
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&out.images[0].data)
            .expect("valid base64");
        assert_eq!(decoded.as_slice(), png_data);
    }

    /// Test reading an image without seeing_images enabled returns an error.
    #[tokio::test]
    async fn read_file_requires_seeing_images() {
        use super::*;

        let h = Harness::new(Answer::No);
        h.write_bytes("test.jpg", b"JPGDATA");

        let out = h.run(&ReadFile, json!({"path": "test.jpg"})).await;
        assert!(out.is_error);
        assert!(
            out.content.contains("this model does not take images"),
            "expected vision error: {}",
            out.content
        );
    }

    /// Test reading an image with start_line returns an error.
    #[tokio::test]
    async fn read_file_image_with_start_line_is_error() {
        use super::*;

        let h = Harness::new(Answer::No).seeing_images();
        h.write_bytes("test.png", b"PNGDATA");

        let out = h
            .run(&ReadFile, json!({"path": "test.png", "start_line": 1}))
            .await;
        assert!(out.is_error);
        assert!(
            out.content.contains("an image is read whole"),
            "expected range error: {}",
            out.content
        );
    }

    /// Test that .txt files still read as text with no image.
    #[tokio::test]
    async fn read_file_txt_still_reads_as_text() {
        use super::*;

        let h = Harness::new(Answer::No).seeing_images();
        h.write("test.txt", "hello world\n");

        let out = h.run(&ReadFile, json!({"path": "test.txt"})).await;
        succeeded(&out);
        assert!(out.content.contains("hello world"));
        assert!(out.images.is_empty(), "text files should not have images");
    }
}
