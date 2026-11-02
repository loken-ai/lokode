use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::sandbox::approval::{Approval, ApprovalRequest, ApprovalScope};
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
    fn name(&self) -> &str {
        "run_tests"
    }
    fn mutates(&self) -> bool {
        true
    }
    fn description(&self) -> &str {
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

        match ctx
            .approve(ApprovalRequest {
                tool: self.name().into(),
                summary: format!("run_tests [{label}]: {program} {}", argv.join(" ")),
                diff_preview: None,
                // The runner comes from the manifest and the target from the model, so
                // an agreement covers the command just shown and no other target.
                scope: ApprovalScope::Command(format!("{program} {}", argv.join(" "))),
            })
            .await
        {
            Approval::Proceed => {}
            Approval::Denied(why) => {
                return Ok(ToolOutput::err(crate::sandbox::approval::denial(why)))
            }
            Approval::Interrupted => return Ok(ToolOutput::err("interrupted by user")),
        }

        // The suite runs the project's own code, so it runs in the same sandbox as the
        // shell.
        let args: Vec<&str> = argv.iter().map(String::as_str).collect();
        let asking = crate::sandbox::confine::Asking {
            gate: std::sync::Arc::new(ctx.clone()),
            origin: format!("run_tests: {program} {}", argv.join(" ")),
        };
        let (mut cmd, held) = match crate::sandbox::confine::command(
            ctx.sandbox.as_deref(),
            Some(asking),
            program,
            &args,
        ) {
            Ok(built) => built,
            Err(e) => {
                return Ok(ToolOutput::err(format!(
                    "the sandbox could not be applied, so the tests did not run: {e}"
                )))
            }
        };
        // A test suite is the longest thing this agent starts.
        // Abandoning the turn has to take the suite with it.
        cmd.kill_on_drop(true);
        cmd.current_dir(&ctx.root);
        // The line that names the failing test arrives long before the end of the suite.
        // Read as it comes, like the shell.
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return Ok(ToolOutput::err(format!("spawn failed: {e}"))),
        };
        // Kept until the suite has ended: the lease is its right to the network.
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let collect = async {
            let (out, err) = tokio::join!(
                crate::tools::bash::read_lines(stdout, ctx, self.name()),
                crate::tools::bash::read_lines(stderr, ctx, self.name())
            );
            (out, err, child.wait().await)
        };
        let (out, err, status) = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => return Ok(ToolOutput::err("interrupted by user")),
            r = tokio::time::timeout(std::time::Duration::from_secs(TIMEOUT_S), collect) => match r {
                Ok(v) => v,
                Err(_) => return Ok(ToolOutput::err(format!("timed out after {TIMEOUT_S}s"))),
            },
        };
        let code = match status {
            Ok(s) => s.code().unwrap_or(-1),
            Err(e) => return Ok(ToolOutput::err(format!("could not wait for it: {e}"))),
        };

        let mut report = format!("[{label}] exit: {code}\n");
        report.push_str(&ctx.clamp(out));
        report.push('\n');
        report.push_str(&ctx.clamp(err));
        if let Some(sandbox) = ctx.sandbox.as_ref().filter(|_| code != 0) {
            report.push_str(&format!("\n{}", sandbox.note()));
            for line in held.turned_away() {
                report.push_str(&format!("\nnetwork refused {line}"));
            }
        }
        Ok(ToolOutput::ok(report))
    }
}

fn detect_runner(
    root: &std::path::Path,
) -> Result<(&'static str, Vec<&'static str>, &'static str)> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::harness::{refused, Answer, Harness};
    use serde_json::json;

    #[tokio::test]
    async fn the_runner_is_chosen_by_what_the_project_carries() {
        let h = Harness::new(Answer::No);
        h.write("Cargo.toml", "");
        assert_eq!(detect_runner(&h.root).unwrap().2, "cargo");

        let h = Harness::new(Answer::No);
        h.write("package.json", "{}");
        assert_eq!(detect_runner(&h.root).unwrap().2, "npm");

        let h = Harness::new(Answer::No);
        h.write("pyproject.toml", "");
        assert_eq!(detect_runner(&h.root).unwrap().2, "pytest");

        let h = Harness::new(Answer::No);
        h.write("go.mod", "");
        assert_eq!(detect_runner(&h.root).unwrap().2, "go");
    }

    #[tokio::test]
    async fn a_project_with_no_recognisable_layout_is_reported() {
        let h = Harness::new(Answer::Yes);
        let out = h.run(&RunTests, json!({})).await;
        refused(&out, "no recognised test manifest");
    }

    #[tokio::test]
    async fn a_declined_run_does_not_happen() {
        let h = Harness::new(Answer::No);
        h.write("Cargo.toml", "");
        let out = h.run(&RunTests, json!({})).await;
        refused(&out, "user denied");
    }

    /// The summary names the command.
    /// Running a project's tests runs the project's code: build scripts, lifecycle hooks, conftest.
    /// The user is agreeing to that.
    #[tokio::test]
    async fn the_user_is_shown_what_will_be_run() {
        let h = Harness::new(Answer::No);
        h.write("Cargo.toml", "");
        let _ = h.run(&RunTests, json!({})).await;
        let asked = h.asked();
        assert_eq!(asked.len(), 1);
        assert!(
            asked[0].summary.contains("cargo test"),
            "{}",
            asked[0].summary
        );
    }

    #[tokio::test]
    async fn an_abandoned_turn_stops_the_run_before_it_asks() {
        let h = Harness::cancelled();
        h.write("Cargo.toml", "");
        let out = h.run(&RunTests, json!({})).await;
        refused(&out, "interrupted");
    }
}
