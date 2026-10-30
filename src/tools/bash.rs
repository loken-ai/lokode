use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

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
    #[serde(default)]
    outside_sandbox: bool,
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
        "Run a shell command. Requires user approval. cwd must be inside project root. It runs \
         in a sandbox where there is one; outside_sandbox asks the user to run it unconfined, \
         for a command the sandbox stopped."
    }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "cmd":       {"type": "string"},
                "cwd":       {"type": "string"},
                "timeout_s": {"type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT_S},
                "outside_sandbox": {"type": "boolean"}
            },
            "required": ["cmd"],
            "additionalProperties": false
        })
    }

    async fn run(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput> {
        let a: BashArgs = serde_json::from_value(args)?;
        let cwd = match a.cwd.as_ref() {
            Some(p) => match allowlist::resolve(&ctx.root, p, true) {
                Ok(resolved) => resolved,
                // The command can always run at the root, which a refused directory names.
                Err(e) => {
                    return Ok(ToolOutput::err(format!(
                        "{}; leave cwd out to run at the project root",
                        allowlist::refusal(&ctx.root, p, &e)
                    )))
                }
            },
            None => ctx.root.clone(),
        };
        let timeout =
            Duration::from_secs(a.timeout_s.unwrap_or(DEFAULT_TIMEOUT_S).min(MAX_TIMEOUT_S));

        let sandbox = ctx.sandbox.as_ref().filter(|_| !a.outside_sandbox);
        let outside = match a.outside_sandbox && ctx.sandbox.is_some() {
            true => "  (outside the sandbox)",
            false => "",
        };
        match ctx
            .approve(ApprovalRequest {
                tool: self.name().into(),
                summary: if cwd == ctx.root {
                    format!("$ {}{outside}", a.cmd)
                } else {
                    format!("$ {}{outside}  (in {})", a.cmd, ctx.short(&cwd))
                },
                diff_preview: None,
                scope: match outside.is_empty() {
                    true => ApprovalScope::Command(a.cmd.clone()),
                    false => ApprovalScope::Unconfined(a.cmd.clone()),
                },
            })
            .await
        {
            Approval::Proceed => {}
            Approval::Denied(why) => {
                return Ok(ToolOutput::err(crate::sandbox::approval::denial(why)))
            }
            Approval::Interrupted => return Ok(ToolOutput::err("interrupted by user")),
        }

        // Kept until the spawn is over.
        // A sandbox that cannot be applied refuses the command rather than run it unconfined.
        let (mut cmd, _held) = match crate::sandbox::confine::command(
            sandbox.map(|s| s.as_ref()),
            "/bin/sh",
            &["-lc", &a.cmd],
        ) {
            Ok(built) => built,
            Err(e) => {
                return Ok(ToolOutput::err(format!(
                    "the sandbox could not be applied, so the command did not run: {e}"
                )))
            }
        };
        // Abandoning the turn drops the future, and nobody is left to read the output.
        // The command is killed with it so that it does not keep running.
        cmd.kill_on_drop(true);
        cmd.current_dir(&cwd);

        // Output is read as it comes rather than at the end.
        // The line that says which test failed is shown before the suite finishes.
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
        if let Some(sandbox) = sandbox.filter(|_| code != 0) {
            report.push_str(&format!("\n{}", sandbox.note()));
        }

        Ok(ToolOutput::ok(report))
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use crate::tools::harness::{Answer, Harness};

    /// A command that prints for a while shows its lines while it prints them.
    /// The result still carries all of them.
    /// The screen sees the tail and the model sees the text.
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
            if let crate::tools::registry::Live::Printed(tail) = printed.what {
                seen = tail.read().lines().map(String::from).collect();
            }
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
        assert!(out.content.contains("leave cwd out"), "{}", out.content);
        assert!(
            h.asked().is_empty(),
            "the user was asked about a command that could not run"
        );
    }

    #[tokio::test]
    async fn a_missing_working_directory_names_the_root_as_the_way_out() {
        let h = Harness::new(Answer::Yes);
        let out = h
            .run(&Bash, json!({"cmd": "ls", "cwd": "/workspace/project"}))
            .await;
        assert!(
            out.content
                .contains("leave cwd out to run at the project root"),
            "{}",
            out.content
        );
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
    /// Otherwise one approved `ls` would be a standing permission to run anything.
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

    /// The whole claim of `--confine-shell`, stated as the thing it prevents.
    /// This is whether the write actually fails.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_confined_command_cannot_write_outside_the_project() {
        if !crate::sandbox::confine::available() {
            return;
        }
        let h = Harness::confined();
        // Somewhere real, outside the root, that this user can otherwise certainly write.
        let outside = std::env::temp_dir().join("lokode-confinement-escape");
        let _ = std::fs::remove_file(&outside);

        let out = h
            .run(
                &Bash,
                json!({"cmd": format!("echo escaped > {}", outside.display())}),
            )
            .await;

        // The shell ran and its redirection was refused.
        // This is a report of a failure, not a refusal to run.
        succeeded(&out);
        assert!(
            !outside.exists(),
            "a confined command wrote to {}",
            outside.display()
        );
        assert!(
            !out.content.contains("exit: 0"),
            "the command claimed to succeed: {}",
            out.content
        );
        let _ = std::fs::remove_file(&outside);
    }

    /// A command the model asks to run outside the sandbox is put to the user as such.
    /// Once agreed, it runs unconfined.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_command_outside_the_sandbox_is_asked_for_and_unconfined() {
        if !crate::sandbox::confine::available() {
            return;
        }
        let h = Harness::confined();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("written");
        let out = h
            .run(
                &Bash,
                json!({"cmd": format!("touch {}", target.display()), "outside_sandbox": true}),
            )
            .await;
        succeeded(&out);
        assert!(target.exists(), "{}", out.content);
        let asked = h.asked();
        assert!(
            matches!(asked[0].scope, ApprovalScope::Unconfined(_)),
            "{:?}",
            asked[0].scope
        );
        assert!(asked[0].summary.ends_with("(outside the sandbox)"));
    }

    /// A confined command may discard its output.
    /// It does not see the variables the sandbox takes out.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_confined_command_writes_to_devices_and_misses_the_key() {
        if !crate::sandbox::confine::available() {
            return;
        }
        std::env::set_var("LOKODE_TEST_SECRET", "hunter2");
        let h = Harness::confined();
        let mut ctx = h.ctx.clone();
        ctx.sandbox = Some(std::sync::Arc::new(crate::sandbox::confine::Sandbox {
            unset: vec!["LOKODE_TEST_SECRET".into()],
            ..(**h.ctx.sandbox.as_ref().unwrap()).clone()
        }));
        let out = Bash
            .run(
                &ctx,
                serde_json::json!({"cmd": "echo gone > /dev/null && echo \"[$LOKODE_TEST_SECRET]\""}),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{out:?}");
        assert!(
            out.content.contains("exit: 0") && out.content.contains("[]"),
            "{}",
            out.content
        );
    }

    /// The control for the test above.
    /// Without it, that one would keep passing if the command had stopped writing for some reason
    /// of its own.
    /// This is also the documented default: unconfined.
    #[tokio::test]
    async fn an_unconfined_command_does_write_outside_the_project() {
        let h = Harness::new(Answer::Yes);
        let outside = std::env::temp_dir().join("lokode-unconfined-control");
        let _ = std::fs::remove_file(&outside);

        let out = h
            .run(
                &Bash,
                json!({"cmd": format!("echo wrote > {}", outside.display())}),
            )
            .await;

        succeeded(&out);
        assert!(out.content.contains("exit: 0"), "{}", out.content);
        assert!(
            outside.exists(),
            "the control never wrote, so the confinement test proves nothing"
        );
        let _ = std::fs::remove_file(&outside);
    }

    /// The other half: confinement that also stops the project working is not usable.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_confined_command_still_writes_inside_the_project() {
        if !crate::sandbox::confine::available() {
            return;
        }
        let h = Harness::confined();
        let out = h.run(&Bash, json!({"cmd": "echo made > inside.txt"})).await;
        succeeded(&out);
        assert!(out.content.contains("exit: 0"), "{}", out.content);
        assert_eq!(h.read("inside.txt").trim(), "made");
    }

    /// Reading is deliberately not confined.
    /// A protection is described by what it does not do as much as by what it does.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_confined_command_can_still_read_outside_the_project() {
        if !crate::sandbox::confine::available() {
            return;
        }
        let h = Harness::confined();
        let out = h.run(&Bash, json!({"cmd": "head -n1 /etc/hostname"})).await;
        succeeded(&out);
        assert!(out.content.contains("exit: 0"), "{}", out.content);
    }

    /// A child cannot hand back what its parent gave up.
    /// The approved command is not the only thing that runs.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn confinement_is_inherited_by_what_the_command_starts() {
        if !crate::sandbox::confine::available() {
            return;
        }
        let h = Harness::confined();
        let outside = std::env::temp_dir().join("lokode-confinement-child-escape");
        let _ = std::fs::remove_file(&outside);

        let out = h
            .run(
                &Bash,
                json!({"cmd": format!("sh -c 'echo escaped > {}'", outside.display())}),
            )
            .await;

        succeeded(&out);
        assert!(
            !outside.exists(),
            "a grandchild of the confined shell wrote to {}",
            outside.display()
        );
        let _ = std::fs::remove_file(&outside);
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
/// Shared with the test runner.
///
/// The whole text is returned.
/// What the model reads is what ran, whatever the screen showed.
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
