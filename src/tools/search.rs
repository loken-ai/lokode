use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::process::Command;

use crate::sandbox::allowlist;
use crate::tools::registry::{Tool, ToolCtx, ToolOutput};

const MAX_OUTPUT_BYTES: usize = 64 * 1024;

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
    fn name(&self) -> &'static str { "grep" }
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

        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        if text.len() > MAX_OUTPUT_BYTES {
            text.truncate(MAX_OUTPUT_BYTES);
            text.push_str("\n… (truncated)");
        }
        if text.is_empty() {
            Ok(ToolOutput::ok("(no matches)"))
        } else {
            Ok(ToolOutput::ok(text))
        }
    }
}
