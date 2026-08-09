use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::process::Command;

use crate::sandbox::allowlist;
use crate::sandbox::approval::{Approval, ApprovalRequest, ApprovalScope};
use crate::tools::registry::{Tool, ToolCtx, ToolOutput};

const DEFAULT_TIMEOUT_S: u64 = 60;
const MAX_TIMEOUT_S: u64 = 600;

pub struct Bash;

#[derive(Deserialize)]
struct BashArgs {
    cmd: String,
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    timeout_s: Option<u64>,
}

#[async_trait]
impl Tool for Bash {
    fn name(&self) -> &'static str { "bash" }
    fn description(&self) -> &'static str {
        "Run a shell command. Requires user approval. cwd must be inside project root."
    }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "cmd":       {"type": "string"},
                "cwd":       {"type": "string"},
                "timeout_s": {"type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT_S}
            },
            "required": ["cmd"],
            "additionalProperties": false
        })
    }

    async fn run(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput> {
        let a: BashArgs = serde_json::from_value(args)?;
        let cwd = match a.cwd.as_ref() {
            Some(p) => match allowlist::resolve(&ctx.root, p, true) {
                Ok(p) => p,
                Err(e) => return Ok(ToolOutput::err(e.to_string())),
            },
            None => ctx.root.clone(),
        };
        let timeout = Duration::from_secs(a.timeout_s.unwrap_or(DEFAULT_TIMEOUT_S).min(MAX_TIMEOUT_S));

        match ctx
            .approve(ApprovalRequest {
                tool: self.name().into(),
                summary: format!("$ {}  (cwd: {})", a.cmd, cwd.display()),
                diff_preview: None,
                scope: ApprovalScope::Command(a.cmd.clone()),
            })
            .await
        {
            Approval::Proceed => {}
            Approval::Denied => return Ok(ToolOutput::err("user denied")),
            Approval::Interrupted => return Ok(ToolOutput::err("interrupted by user")),
        }

        let mut cmd = Command::new("/bin/sh");
        // Without this, abandoning the turn leaves the command running: the future is
        // dropped, nobody is left to read its output, and a build carries on burning the
        // machine the user just told us to stop using.
        cmd.kill_on_drop(true);
        cmd.arg("-lc").arg(&a.cmd).current_dir(&cwd);

        let fut = cmd.output();
        let output = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => return Ok(ToolOutput::err("interrupted by user")),
            r = tokio::time::timeout(timeout, fut) => match r {
                Ok(Ok(o)) => o,
                Ok(Err(e)) => return Ok(ToolOutput::err(format!("spawn failed: {e}"))),
                Err(_) => {
                    return Ok(ToolOutput::err(format!("timed out after {}s", timeout.as_secs())))
                }
            },
        };

        let mut report = String::new();
        report.push_str(&format!("exit: {}\n", output.status.code().unwrap_or(-1)));
        report.push_str("stdout:\n");
        report.push_str(&ctx.clamp(String::from_utf8_lossy(&output.stdout).into()));
        report.push_str("\nstderr:\n");
        report.push_str(&ctx.clamp(String::from_utf8_lossy(&output.stderr).into()));

        Ok(ToolOutput::ok(report))
    }
}

