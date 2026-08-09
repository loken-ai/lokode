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
    fn name(&self) -> &str {
        "bash"
    }
    fn mutates(&self) -> bool {
        true
    }
    fn description(&self) -> &str {
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
        let timeout =
            Duration::from_secs(a.timeout_s.unwrap_or(DEFAULT_TIMEOUT_S).min(MAX_TIMEOUT_S));

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::approval::ApprovalScope;
    use crate::tools::harness::{refused, succeeded, Answer, Harness};
    use serde_json::json;

    #[tokio::test]
    async fn a_command_runs_and_reports_its_output_and_status() {
        let h = Harness::new(Answer::Yes);
        let out = h.run(&Bash, json!({"cmd": "echo hello; exit 3"})).await;
        succeeded(&out);
        assert!(out.content.contains("exit: 3"), "{}", out.content);
        assert!(out.content.contains("hello"), "{}", out.content);
    }

    #[tokio::test]
    async fn a_declined_command_does_not_run() {
        let h = Harness::new(Answer::No);
        let out = h
            .run(
                &Bash,
                json!({"cmd": format!("touch {}", h.path("ran").display())}),
            )
            .await;
        refused(&out, "user denied");
        assert!(!h.exists("ran"), "the command ran despite being declined");
    }

    #[tokio::test]
    async fn a_command_runs_in_the_root_by_default() {
        let h = Harness::new(Answer::Yes);
        h.write("marker.txt", "");
        let out = h.run(&Bash, json!({"cmd": "ls"})).await;
        succeeded(&out);
        assert!(out.content.contains("marker.txt"), "{}", out.content);
    }

    #[tokio::test]
    async fn a_working_directory_outside_the_root_is_refused() {
        let h = Harness::new(Answer::Yes);
        let out = h.run(&Bash, json!({"cmd": "ls", "cwd": "/etc"})).await;
        refused(&out, "outside the project root");
        assert!(
            h.asked().is_empty(),
            "the user was asked about a command that could not run"
        );
    }

    #[tokio::test]
    async fn a_command_that_overruns_its_time_is_stopped() {
        let h = Harness::new(Answer::Yes);
        let out = h
            .run(&Bash, json!({"cmd": "sleep 5", "timeout_s": 1}))
            .await;
        refused(&out, "timed out");
    }

    /// The scope of an `Always` here is the command as shown, not the shell in general.
    /// Anything else and one approved `ls` is a standing permission to run anything.
    #[tokio::test]
    async fn agreeing_always_covers_only_that_command() {
        let h = Harness::new(Answer::Always);
        succeeded(&h.run(&Bash, json!({"cmd": "true"})).await);
        assert_eq!(h.asked()[0].scope, ApprovalScope::Command("true".into()));

        // The same command again: no second question.
        succeeded(&h.run(&Bash, json!({"cmd": "true"})).await);
        assert_eq!(h.asked().len(), 1);

        // A different one, however similar: asked.
        succeeded(&h.run(&Bash, json!({"cmd": "true; echo more"})).await);
        assert_eq!(
            h.asked().len(),
            2,
            "a different command inherited the agreement"
        );
    }

    #[tokio::test]
    async fn an_abandoned_turn_stops_the_command_before_it_asks() {
        let h = Harness::cancelled();
        let out = h
            .run(
                &Bash,
                json!({"cmd": format!("touch {}", h.path("ran").display())}),
            )
            .await;
        refused(&out, "interrupted");
        assert!(!h.exists("ran"));
    }
}
