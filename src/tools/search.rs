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
    fn name(&self) -> &'static str {
        "grep"
    }
    fn description(&self) -> &'static str {
        "Search files for a regex (or literal with fixed_string=true). Uses ripgrep if available."
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
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ToolOutput::err(
                    "ripgrep (rg) not found; install ripgrep to enable grep",
                ));
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
