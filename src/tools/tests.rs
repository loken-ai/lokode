use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::process::Command;

use crate::sandbox::approval::{self, ApprovalDecision, ApprovalRequest};
use crate::tools::registry::{Tool, ToolCtx, ToolOutput};

const TIMEOUT_S: u64 = 600;

pub struct RunTests;

#[derive(Deserialize)]
struct Args {
    /// Specific test target/file/name to pass to the underlying runner.
    #[serde(default)]
    target: Option<String>,
}

#[async_trait]
impl Tool for RunTests {
    fn name(&self) -> &'static str { "run_tests" }
    fn description(&self) -> &'static str {
        "Auto-detect project layout (Cargo.toml / package.json / pyproject.toml) and run the test suite."
    }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"target": {"type": "string"}},
            "additionalProperties": false
        })
    }

    async fn run(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput> {
        let a: Args = serde_json::from_value(args)?;
        let (program, base_args, label) = detect_runner(&ctx.root)?;
        let mut argv: Vec<String> = base_args.into_iter().map(String::from).collect();
        if let Some(t) = a.target.as_ref() {
            argv.push(t.clone());
        }

        if !ctx.session_approvals.is_always(self.name()).await {
            let decision = approval::ask(
                &ctx.approval,
                ApprovalRequest {
                    tool: self.name().into(),
                    summary: format!("run_tests [{label}]: {program} {}", argv.join(" ")),
                    diff_preview: None,
                },
            )
            .await;
            match decision {
                ApprovalDecision::Deny => return Ok(ToolOutput::err("user denied")),
                ApprovalDecision::Always => ctx.session_approvals.mark_always(self.name()).await,
                ApprovalDecision::Once => {}
            }
        }

        let mut cmd = Command::new(program);
        cmd.args(&argv).current_dir(&ctx.root);
        let fut = cmd.output();
        let output = match tokio::time::timeout(std::time::Duration::from_secs(TIMEOUT_S), fut).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return Ok(ToolOutput::err(format!("spawn failed: {e}"))),
            Err(_) => return Ok(ToolOutput::err(format!("timed out after {TIMEOUT_S}s"))),
        };

        let mut report = format!("[{label}] exit: {}\n", output.status.code().unwrap_or(-1));
        report.push_str(&ctx.clamp(String::from_utf8_lossy(&output.stdout).into()));
        report.push('\n');
        report.push_str(&ctx.clamp(String::from_utf8_lossy(&output.stderr).into()));
        Ok(ToolOutput::ok(report))
    }
}

fn detect_runner(root: &std::path::Path) -> Result<(&'static str, Vec<&'static str>, &'static str)> {
    if root.join("Cargo.toml").exists() {
        return Ok(("cargo", vec!["test", "--quiet"], "cargo"));
    }
    if root.join("package.json").exists() {
        return Ok(("npm", vec!["test", "--silent"], "npm"));
    }
    if root.join("pyproject.toml").exists() || root.join("pytest.ini").exists() {
        return Ok(("pytest", vec!["-q"], "pytest"));
    }
    if root.join("go.mod").exists() {
        return Ok(("go", vec!["test", "./..."], "go"));
    }
    anyhow::bail!("no recognised test manifest at {}", root.display())
}

