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
}

#[async_trait]
impl Tool for Grep {
    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> &str {
        "Search files for a regex (or literal with fixed_string=true), honouring ignore files."
    }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern":          {"type": "string"},
                "path":             {"type": "string"},
                "case_insensitive": {"type": "boolean"},
                "fixed_string":     {"type": "boolean"}
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }

    async fn run(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput> {
        let a: GrepArgs = serde_json::from_value(args)?;
        let search_root = match a.path.as_ref() {
            Some(p) => match allowlist::resolve(&ctx.root, p, true) {
                Ok(p) => p,
                Err(e) => return Ok(ToolOutput::err(e.to_string())),
            },
            None => ctx.root.clone(),
        };

        let mut cmd = Command::new("rg");
        cmd.arg("--line-number")
            .arg("--no-heading")
            .arg("--color=never")
            .arg("--max-count=200");
        if a.case_insensitive {
            cmd.arg("-i");
        }
        if a.fixed_string {
            cmd.arg("-F");
        }
        cmd.arg("--").arg(&a.pattern).arg(&search_root);

        let output = match cmd.output().await {
            Ok(o) => o,
            // ripgrep is faster and knows more about what to skip, but it was an undocumented
            // hard requirement: without it on the path this tool simply did not work, and the
            // agent lost the ability to search at all.
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

        // ripgrep exit code 1 = no matches; treat as empty success.
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

    /// No matches is an answer, not a failure: ripgrep exits 1 for it, and treating that
    /// as an error would tell the model its search was broken.
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
/// The same shape of answer - `path:line:text` - from the same walk `glob` uses, so ignore
/// files are still honoured and a build directory is still skipped. Slower than ripgrep and
/// not trying to be otherwise: the point is that the tool works on a machine that does not
/// have it.
async fn search_here(
    ctx: &ToolCtx,
    root: &std::path::Path,
    args: &GrepArgs,
) -> Result<String, String> {
    let pattern = if args.fixed_string {
        regex::escape(&args.pattern)
    } else {
        args.pattern.clone()
    };
    let re = regex::RegexBuilder::new(&pattern)
        .case_insensitive(args.case_insensitive)
        .build()
        .map_err(|e| format!("bad pattern: {e}"))?;

    let root = root.to_path_buf();
    let project = ctx.root.clone();
    tokio::task::spawn_blocking(move || {
        use std::io::{BufRead, BufReader};
        let mut out = String::new();
        let mut hits = 0;

        for entry in ignore::WalkBuilder::new(&root).build().flatten() {
            if hits >= MAX_HITS {
                out.push_str("… (truncated)\n");
                break;
            }
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

            // Read by line rather than whole: a match is a line, and a file too big to hold
            // is exactly the kind this has to keep working on.
            for (n, line) in BufReader::new(file).lines().enumerate() {
                // A line that will not decode is a binary file saying so.
                let Ok(line) = line else { break };
                if re.is_match(&line) {
                    out.push_str(&format!("{shown}:{}:{line}\n", n + 1));
                    hits += 1;
                    if hits >= MAX_HITS {
                        break;
                    }
                }
            }
        }
        out
    })
    .await
    .map_err(|e| format!("search failed: {e}"))
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
        }
    }

    /// The fallback exists so the tool works on a machine without ripgrep, which means it
    /// has to answer in the same shape.
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
}
