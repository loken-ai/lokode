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

        // Built before the fork: creating the ruleset allocates and opens files, and doing
        // that between fork and exec can deadlock. What the child runs is two syscalls.
        let ruleset = match &ctx.shell_writable {
            None => None,
            Some(writable) => match crate::sandbox::confine::ruleset(writable) {
                Ok(fd) => Some(fd),
                Err(e) => {
                    // Confinement was asked for and cannot be given. Running the command
                    // anyway would be doing the opposite of what was asked.
                    return Ok(ToolOutput::err(format!(
                        "shell confinement was requested and could not be applied: {e}"
                    )));
                }
            },
        };

        let mut cmd = Command::new("/bin/sh");
        // Without this, abandoning the turn leaves the command running: the future is
        // dropped, nobody is left to read its output, and a build carries on burning the
        // machine the user just told us to stop using.
        cmd.kill_on_drop(true);
        cmd.arg("-lc").arg(&a.cmd).current_dir(&cwd);

        if let Some(fd) = &ruleset {
            use std::os::fd::AsRawFd;
            let raw = fd.as_raw_fd();
            // SAFETY: `restrict_after_fork` makes two syscalls and nothing else - no
            // allocation, no locks - which is what a post-fork hook may do. `raw` stays
            // open because `ruleset` outlives the spawn below.
            unsafe {
                cmd.pre_exec(move || crate::sandbox::confine::restrict_after_fork(raw));
            }
        }

        // Read as it comes rather than at the end: a build that says nothing for two
        // minutes cannot be told from one that has hung, and the line that says which
        // test failed is worth having before the suite finishes.
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return Ok(ToolOutput::err(format!("spawn failed: {e}"))),
        };
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let collect = async {
            let (out, err) = tokio::join!(
                read_lines(stdout, ctx, self.name()),
                read_lines(stderr, ctx, self.name())
            );
            let status = child.wait().await;
            (out, err, status)
        };

        let (out, err, status) = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => return Ok(ToolOutput::err("interrupted by user")),
            r = tokio::time::timeout(timeout, collect) => match r {
                Ok(v) => v,
                Err(_) => {
                    return Ok(ToolOutput::err(format!("timed out after {}s", timeout.as_secs())))
                }
            },
        };
        let code = match status {
            Ok(s) => s.code().unwrap_or(-1),
            Err(e) => return Ok(ToolOutput::err(format!("could not wait for it: {e}"))),
        };

        let mut report = String::new();
        report.push_str(&format!("exit: {code}\n"));
        report.push_str("stdout:\n");
        report.push_str(&ctx.clamp(out));
        report.push_str("\nstderr:\n");
        report.push_str(&ctx.clamp(err));

        Ok(ToolOutput::ok(report))
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use crate::tools::harness::{Answer, Harness};

    /// A command that prints for a while shows its lines while it prints them, and the
    /// result still carries all of them: the screen sees the tail, the model sees the text.
    #[tokio::test]
    async fn a_command_shows_its_output_while_it_runs() {
        let h = Harness::new(Answer::Yes);
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let mut ctx = h.ctx.clone();
        ctx.live = Some(tx);

        let out = Bash
            .run(
                &ctx,
                serde_json::json!({"cmd": "for i in 1 2 3; do echo line $i; done; echo oops >&2"}),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{out:?}");

        let mut seen = Vec::new();
        while let Ok(printed) = rx.try_recv() {
            assert_eq!(printed.tool, "bash");
            seen.push(printed.line);
        }
        assert!(seen.contains(&"line 1".to_string()), "{seen:?}");
        assert!(seen.contains(&"line 3".to_string()), "{seen:?}");
        assert!(seen.contains(&"oops".to_string()), "{seen:?}");
        // What the model reads is still the whole of it.
        assert!(out.content.contains("line 1"));
        assert!(out.content.contains("line 3"));
        assert!(out.content.contains("oops"));
        assert!(out.content.contains("exit: 0"));
    }

    /// A caller with no screen is not sent lines it would only put in a log.
    #[tokio::test]
    async fn nothing_is_sent_when_nobody_is_watching() {
        let h = Harness::new(Answer::Yes);
        let out = Bash
            .run(&h.ctx, serde_json::json!({"cmd": "echo quiet"}))
            .await
            .unwrap();
        assert!(out.content.contains("quiet"));
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

/// Read one of a child's streams to the end, showing each line as it arrives.
///
/// Shared with the test runner, which has the same problem in a longer form.
///
/// The whole text is still returned: what the model reads has to be what ran, and a
/// screen that showed the last twenty lines has not told it about the first hundred.
pub(crate) async fn read_lines(
    stream: Option<impl tokio::io::AsyncRead + Unpin>,
    ctx: &ToolCtx,
    tool: &str,
) -> String {
    use tokio::io::AsyncBufReadExt;
    let Some(stream) = stream else {
        return String::new();
    };
    let mut lines = tokio::io::BufReader::new(stream).lines();
    let mut all = String::new();
    while let Ok(Some(line)) = lines.next_line().await {
        ctx.show(tool, &line);
        all.push_str(&line);
        all.push('\n');
    }
    all
}
