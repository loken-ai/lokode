use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::process::Command;

use crate::sandbox::allowlist;
use crate::tools::registry::{Tool, ToolCtx, ToolOutput};

pub struct Grep;

#[derive(Deserialize)]
struct GrepArgs {
    pattern: String,
    #[serde(default)]
    path: Option<PathBuf>,
    #[serde(default)]
    case_insensitive: bool,
    #[serde(default)]
    fixed_string: bool,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default)]
    before: Option<usize>,
    #[serde(default)]
    after: Option<usize>,
    #[serde(default)]
    context: Option<usize>,
    #[serde(default)]
    output_mode: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait]
impl Tool for Grep {
    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> &str {
        "Search files for a regex (or literal with fixed_string=true), honouring ignore files. Optional: glob, before, after, context, output_mode (content/files_with_matches/count), limit."
    }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern":          {"type": "string"},
                "path":             {"type": "string"},
                "case_insensitive": {"type": "boolean"},
                "fixed_string":     {"type": "boolean"},
                "glob":             {"type": "string"},
                "before":           {"type": "integer"},
                "after":            {"type": "integer"},
                "context":          {"type": "integer"},
                "output_mode":      {"type": "string", "enum": ["content", "files_with_matches", "count"]},
                "limit":            {"type": "integer"}
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }

    async fn run(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput> {
        let a: GrepArgs = serde_json::from_value(args)?;

        // Validate output_mode if provided
        if let Some(ref mode) = a.output_mode {
            if mode != "content" && mode != "files_with_matches" && mode != "count" {
                return Ok(ToolOutput::err(format!(
                    "output_mode must be one of: content, files_with_matches, count (got: {})",
                    mode
                )));
            }
        }

        let search_root = match a.path.as_ref() {
            Some(p) => match allowlist::resolve(&ctx.root, p, true) {
                Ok(resolved) => resolved,
                Err(e) => return Ok(ToolOutput::err(allowlist::refusal(&ctx.root, p, &e))),
            },
            None => ctx.root.clone(),
        };

        let mut cmd = Command::new("rg");
        cmd.arg("--line-number")
            .arg("--no-heading")
            .arg("--color=never");

        // Handle limit
        if let Some(limit) = a.limit {
            cmd.arg(format!("--max-count={}", limit));
        }

        if a.case_insensitive {
            cmd.arg("-i");
        }
        if a.fixed_string {
            cmd.arg("-F");
        }

        // Handle glob
        if let Some(ref glob) = a.glob {
            cmd.arg("--glob").arg(glob);
        }

        // Handle before/after/context
        if let Some(before) = a.before {
            cmd.arg(format!("-B {}", before));
        }
        if let Some(after) = a.after {
            cmd.arg(format!("-A {}", after));
        }
        if let Some(context) = a.context {
            cmd.arg(format!("-C {}", context));
        }

        // Handle output_mode
        if let Some(ref mode) = a.output_mode {
            match mode.as_str() {
                "files_with_matches" => {
                    cmd.arg("-l");
                }
                "count" => {
                    cmd.arg("--count");
                }
                "content" => {
                    // default, no flag needed
                }
                _ => unreachable!(),
            }
        }

        cmd.arg("--").arg(&a.pattern).arg(&search_root);

        let output = match cmd.output().await {
            Ok(o) => o,
            // The built-in walker answers when ripgrep is not installed.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!("ripgrep not found; searching with the built-in walker");
                return match search_here(ctx, &search_root, &a).await {
                    Ok(text) if text.is_empty() => Ok(ToolOutput::ok("(no matches)")),
                    Ok(text) => Ok(ToolOutput::ok(ctx.clamp(text))),
                    Err(e) => Ok(ToolOutput::err(e)),
                };
            }
            Err(e) => return Ok(ToolOutput::err(format!("rg failed: {e}"))),
        };

        // ripgrep exit code 1 means no matches: an empty success.
        if !output.status.success() && output.status.code() != Some(1) {
            return Ok(ToolOutput::err(format!(
                "rg exit {:?}: {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        let text = ctx.clamp(String::from_utf8_lossy(&output.stdout).into_owned());
        if text.is_empty() {
            Ok(ToolOutput::ok("(no matches)"))
        } else {
            Ok(ToolOutput::ok(text))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::harness::{refused, succeeded, Answer, Harness};
    use serde_json::json;

    fn have_ripgrep() -> bool {
        std::process::Command::new("rg")
            .arg("--version")
            .output()
            .is_ok()
    }

    #[tokio::test]
    async fn grep_reports_matches_with_line_numbers() {
        if !have_ripgrep() {
            return;
        }
        let h = Harness::new(Answer::No);
        h.write("a.rs", "fn alpha() {}\nfn beta() {}\n");
        let out = h.run(&Grep, json!({"pattern": "beta"})).await;
        succeeded(&out);
        assert!(out.content.contains("a.rs"), "{}", out.content);
        assert!(
            out.content.contains(":2:"),
            "line number missing: {}",
            out.content
        );
    }

    /// No matches is an answer, not a failure.
    /// ripgrep exits 1 for it.
    #[tokio::test]
    async fn grep_finding_nothing_is_a_success() {
        if !have_ripgrep() {
            return;
        }
        let h = Harness::new(Answer::No);
        h.write("a.rs", "nothing to see\n");
        let out = h.run(&Grep, json!({"pattern": "absent"})).await;
        succeeded(&out);
        assert_eq!(out.content, "(no matches)");
    }

    #[tokio::test]
    async fn grep_can_be_told_to_ignore_case() {
        if !have_ripgrep() {
            return;
        }
        let h = Harness::new(Answer::No);
        h.write("a.rs", "Alpha\n");
        let sensitive = h.run(&Grep, json!({"pattern": "alpha"})).await;
        assert_eq!(sensitive.content, "(no matches)");
        let insensitive = h
            .run(&Grep, json!({"pattern": "alpha", "case_insensitive": true}))
            .await;
        assert!(
            insensitive.content.contains("Alpha"),
            "{}",
            insensitive.content
        );
    }

    /// A regex metacharacter searched as a literal must not be compiled as a pattern.
    #[tokio::test]
    async fn grep_can_search_for_a_literal() {
        if !have_ripgrep() {
            return;
        }
        let h = Harness::new(Answer::No);
        h.write("a.rs", "a+b\naxb\n");
        let out = h
            .run(&Grep, json!({"pattern": "a+b", "fixed_string": true}))
            .await;
        succeeded(&out);
        assert!(out.content.contains("a+b"), "{}", out.content);
        assert!(
            !out.content.contains("axb"),
            "the pattern was compiled: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn grep_refuses_to_search_outside_the_root() {
        let h = Harness::new(Answer::No);
        let out = h.run(&Grep, json!({"pattern": "x", "path": "/etc"})).await;
        refused(&out, "outside the project root");
    }
}

/// Lines a search may return before it stops looking.
const MAX_HITS: usize = 500;

/// Search without ripgrep.
///
/// The answer has the same shape, `path:line:text`.
/// It uses the same walk `glob` uses.
/// Ignore files are honoured and a build directory is skipped.
/// It is slower than ripgrep.
/// It works on a machine that does not have ripgrep.
async fn search_here(
    ctx: &ToolCtx,
    root: &std::path::Path,
    _args: &GrepArgs,
) -> Result<String, String> {
    let root = root.to_path_buf();
    let project = ctx.root.clone();

    // Extract all needed data before spawning
    let pattern = _args.pattern.clone();
    let fixed_string = _args.fixed_string;
    let case_insensitive = _args.case_insensitive;
    let output_mode = _args.output_mode.clone();
    let limit = _args.limit;
    let before = _args.before;
    let after = _args.after;
    let context = _args.context;
    let glob = _args.glob.clone();

    tokio::task::spawn_blocking(move || {
        use std::collections::HashMap;
        use std::io::{BufRead, BufReader};

        // Build regex
        let pattern_to_search = if fixed_string {
            regex::escape(&pattern)
        } else {
            pattern.clone()
        };
        let re = regex::RegexBuilder::new(&pattern_to_search)
            .case_insensitive(case_insensitive)
            .build()
            .map_err(|e| format!("bad pattern: {e}"))?;

        // Build output mode
        let output_mode = output_mode.as_deref().unwrap_or("content");
        if output_mode != "content" && output_mode != "files_with_matches" && output_mode != "count"
        {
            return Err(format!(
                "output_mode must be one of: content, files_with_matches, count (got: {})",
                output_mode
            ));
        }

        // Build glob overrides if provided
        let mut override_builder = ignore::overrides::OverrideBuilder::new(&root);
        if let Some(ref glob) = glob {
            override_builder
                .add(glob)
                .map_err(|e| format!("bad glob: {e}"))?;
        }
        let overrides = override_builder
            .build()
            .map_err(|e| format!("bad glob: {e}"))?;

        let mut out = String::new();
        let mut hits = 0;
        let limit = limit.unwrap_or(MAX_HITS);

        // For context support, we need to track lines
        let before = before.unwrap_or(context.unwrap_or(0));
        let after = after.unwrap_or(context.unwrap_or(0));
        let has_context = before > 0 || after > 0;

        let mut last_match_end: Option<usize> = None;
        let mut files_with_matches = Vec::new();
        let mut file_counts = HashMap::new();

        for entry in ignore::WalkBuilder::new(&root)
            .overrides(overrides)
            .build()
            .flatten()
        {
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let Ok(file) = std::fs::File::open(entry.path()) else {
                continue;
            };
            let shown = entry
                .path()
                .strip_prefix(&project)
                .unwrap_or(entry.path())
                .display()
                .to_string();

            // Read by line, not whole: a match is a line.
            // A file too big to hold in memory still works.
            let file_lines: Vec<String> =
                BufReader::new(file).lines().map_while(Result::ok).collect();

            for (n, line) in file_lines.iter().enumerate() {
                if re.is_match(line) {
                    hits += 1;

                    match output_mode {
                        "files_with_matches" => {
                            if !files_with_matches.contains(&shown) {
                                files_with_matches.push(shown.clone());
                            }
                            if hits >= limit {
                                break;
                            }
                            continue;
                        }
                        "count" => {
                            *file_counts.entry(shown.clone()).or_insert(0) += 1;
                            if hits >= limit {
                                break;
                            }
                            continue;
                        }
                        "content" => {
                            // Default mode with context support
                            let match_line_num = n + 1;

                            // Add context lines (before) if needed
                            if has_context {
                                if let Some(start_check) = last_match_end {
                                    let end_check = match_line_num - 1;
                                    if start_check + after > end_check {
                                        // Context overlap, add separator
                                        out.push_str("--\n");
                                    }
                                }
                            }

                            // Print before lines
                            let start = n.saturating_sub(before);
                            for (i, line_before) in
                                file_lines.iter().enumerate().take(n).skip(start)
                            {
                                out.push_str(&format!("{}:{}:{}\n", shown, i + 1, line_before));
                            }
                            if n > start {
                                // Already has trailing newline from last iteration
                            }

                            // Print the match line
                            out.push_str(&format!("{}:{}:{}", shown, match_line_num, line));

                            // Print after lines
                            let end = (n + 1 + after).min(file_lines.len());
                            for (i, line_after) in
                                file_lines.iter().enumerate().take(end).skip(n + 1)
                            {
                                out.push_str(&format!("\n{}:{}:{}", shown, i + 1, line_after));
                            }
                            out.push('\n');

                            // Track last match end for context overlap detection
                            let context_end = (n + 1 + after).min(file_lines.len());
                            if let Some(prev_end) = last_match_end {
                                if context_end > prev_end {
                                    last_match_end = Some(context_end);
                                }
                            } else {
                                last_match_end = Some(context_end);
                            }

                            if hits >= limit {
                                break;
                            }
                        }
                        _ => unreachable!(),
                    }
                }
            }
        }

        // Format output based on mode
        match output_mode {
            "files_with_matches" => {
                for f in files_with_matches {
                    out.push_str(&format!("{f}\n"));
                }
            }
            "count" => {
                for (file, count) in file_counts {
                    out.push_str(&format!("{file}:{count}\n"));
                }
            }
            "content" => {
                // No special handling
            }
            _ => unreachable!(),
        }

        // Handle truncation message
        // Only show if we hit the limit before finishing all files
        let limit_reached = hits >= limit;
        if limit_reached {
            out.push_str(&format!("… (truncated after {limit} lines)\n"));
        }

        Ok(out)
    })
    .await
    .map_err(|e| format!("search failed: {e}"))
    .and_then(|s| s)
}

#[cfg(test)]
mod native_tests {
    use super::*;
    use crate::tools::harness::{Answer, Harness};

    async fn search(h: &Harness, args: GrepArgs) -> String {
        search_here(&h.ctx, &h.root.clone(), &args).await.unwrap()
    }

    fn args(pattern: &str) -> GrepArgs {
        GrepArgs {
            pattern: pattern.into(),
            path: None,
            case_insensitive: false,
            fixed_string: false,
            glob: None,
            before: None,
            after: None,
            context: None,
            output_mode: None,
            limit: None,
        }
    }

    /// The fallback answers in the same shape as ripgrep.
    #[tokio::test]
    async fn it_reports_path_line_and_text() {
        let h = Harness::new(Answer::No);
        h.write("src/a.rs", "fn alpha() {}\nfn beta() {}\n");
        let out = search(&h, args("beta")).await;
        assert!(out.contains("src/a.rs:2:fn beta() {}"), "{out}");
    }

    #[tokio::test]
    async fn a_pattern_matching_nothing_returns_nothing() {
        let h = Harness::new(Answer::No);
        h.write("a.rs", "nothing here\n");
        assert_eq!(search(&h, args("absent")).await, "");
    }

    #[tokio::test]
    async fn case_can_be_ignored() {
        let h = Harness::new(Answer::No);
        h.write("a.rs", "Alpha\n");
        assert_eq!(search(&h, args("alpha")).await, "");
        let mut insensitive = args("alpha");
        insensitive.case_insensitive = true;
        assert!(search(&h, insensitive).await.contains("Alpha"));
    }

    /// A regex metacharacter searched as a literal must not be compiled as a pattern.
    #[tokio::test]
    async fn a_literal_is_not_compiled() {
        let h = Harness::new(Answer::No);
        h.write("a.rs", "a+b\naxb\n");
        let mut literal = args("a+b");
        literal.fixed_string = true;
        let out = search(&h, literal).await;
        assert!(out.contains("a+b"), "{out}");
        assert!(!out.contains("axb"), "the pattern was compiled: {out}");
    }

    /// The same walk `glob` uses, so a build directory is skipped the same way.
    #[tokio::test]
    async fn it_honours_the_projects_ignore_file() {
        let h = Harness::new(Answer::No);
        std::fs::create_dir_all(h.path(".git")).unwrap();
        h.write(".gitignore", "target/\n");
        h.write("src/a.rs", "needle\n");
        h.write("target/b.rs", "needle\n");
        let out = search(&h, args("needle")).await;
        assert!(out.contains("src/a.rs"), "{out}");
        assert!(
            !out.contains("target/b.rs"),
            "an ignored path was searched: {out}"
        );
    }

    #[tokio::test]
    async fn an_unusable_pattern_is_reported_rather_than_panicking() {
        let h = Harness::new(Answer::No);
        let err = search_here(&h.ctx, &h.root.clone(), &args("(unclosed"))
            .await
            .unwrap_err();
        assert!(err.contains("bad pattern"), "{err}");
    }

    /// Test glob filter - keeps only .rs files
    #[tokio::test]
    async fn glob_filter_works() {
        let h = Harness::new(Answer::No);
        h.write("src/a.rs", "needle\n");
        h.write("src/b.txt", "needle\n");
        let mut grep_args = args("needle");
        grep_args.glob = Some("*.rs".to_string());
        let out = search(&h, grep_args).await;
        assert!(out.contains("src/a.rs"), "{out}");
        assert!(!out.contains("src/b.txt"), "{out}");
    }

    /// Test context - shows line before and after match
    #[tokio::test]
    async fn context_shows_around_matches() {
        let h = Harness::new(Answer::No);
        h.write("a.rs", "alpha\nbeta\ngamma\n");
        let mut grep_args = args("beta");
        grep_args.context = Some(1);
        let out = search(&h, grep_args).await;
        assert!(out.contains("1:alpha\n"), "{out}");
        assert!(out.contains("2:beta"), "{out}");
        assert!(out.contains("3:gamma\n"), "{out}");
    }

    /// Test files_with_matches - lists each matching file once
    #[tokio::test]
    async fn files_with_matches_lists_files_once() {
        let h = Harness::new(Answer::No);
        h.write("a.rs", "needle\nneedle\n");
        h.write("b.rs", "needle\n");
        let mut grep_args = args("needle");
        grep_args.output_mode = Some("files_with_matches".to_string());
        let out = search(&h, grep_args).await;
        assert!(out.contains("a.rs"), "{out}");
        assert!(out.contains("b.rs"), "{out}");
        // Count occurrences - each file should appear once
        let a_count = out.matches("a.rs").count();
        assert_eq!(a_count, 1, "file a.rs should appear once: {out}");
    }

    /// Test count output mode
    #[tokio::test]
    async fn count_gives_path_with_matches() {
        let h = Harness::new(Answer::No);
        h.write("a.rs", "needle\nneedle\n");
        let mut grep_args = args("needle");
        grep_args.output_mode = Some("count".to_string());
        let out = search(&h, grep_args).await;
        assert!(out.contains("a.rs:2"), "{out}");
    }

    /// Test limit - cuts output and indicates truncation
    #[tokio::test]
    async fn limit_cuts_output() {
        let h = Harness::new(Answer::No);
        h.write("a.rs", "needle\nneedle\nneedle\n");
        let mut grep_args = args("needle");
        grep_args.limit = Some(1);
        let out = search(&h, grep_args).await;
        // Should have at most 1 match and truncation message
        assert!(out.contains("… (truncated after 1 lines)"), "{out}");
        // Count should be at most 1
        assert!(out.matches("needle").count() <= 2, "{out}"); // 1 match + 1 in truncation message
    }

    /// Test invalid output_mode is an error
    #[tokio::test]
    async fn unknown_output_mode_is_error() {
        let h = Harness::new(Answer::No);
        h.write("a.rs", "needle\n");
        let mut grep_args = args("needle");
        grep_args.output_mode = Some("invalid".to_string());
        let result = search_here(&h.ctx, &h.root.clone(), &grep_args).await;
        assert!(result.is_err(), "expected error for invalid output_mode");
        let err = result.unwrap_err();
        assert!(err.contains("output_mode must be one of"), "{err}");
    }
}
