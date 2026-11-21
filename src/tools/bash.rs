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
    /// Start the command and answer at once; its output is read with process_output.
    #[serde(default)]
    background: bool,
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
        match crate::shell::Shell::current().dialect {
            crate::shell::Dialect::Posix => {
                "Run a shell command. Requires user approval. cwd must be inside project root. It \
                 runs in a sandbox where there is one; outside_sandbox asks the user to run it \
                 unconfined, for a command the sandbox stopped. background starts it and answers \
                 at once with a process id for process_output and process_stop."
            }
            crate::shell::Dialect::PowerShell => {
                "Run a PowerShell command. Requires user approval. cwd must be inside project \
                 root. It runs in a sandbox where there is one; outside_sandbox asks the user to \
                 run it unconfined, for a command the sandbox stopped."
            }
        }
    }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "cmd":       {"type": "string"},
                "cwd":       {"type": "string"},
                "timeout_s": {"type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT_S},
                "outside_sandbox": {"type": "boolean"},
                "background": {"type": "boolean"}
            },
            "required": ["cmd"],
            "additionalProperties": false
        })
    }

    async fn run(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput> {
        let a: BashArgs = serde_json::from_value(args)?;
        if a.background && cfg!(not(unix)) {
            return Ok(ToolOutput::err(
                "background commands are not supported here",
            ));
        }
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
        let asking = crate::sandbox::confine::Asking {
            gate: std::sync::Arc::new(ctx.clone()),
            origin: format!("bash: {}", a.cmd.lines().next().unwrap_or_default()),
        };
        let shell = crate::shell::Shell::current();
        let program = shell.program.to_string_lossy().to_string();
        let shell_args = shell.args(&a.cmd, true);
        let shell_args: Vec<&str> = shell_args.iter().map(String::as_str).collect();
        let (mut cmd, _held) = match crate::sandbox::confine::command(
            sandbox.map(|s| s.as_ref()),
            Some(asking),
            &program,
            &shell_args,
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
        own_group(&mut cmd);
        cmd.current_dir(&cwd);

        // Output is read as it comes rather than at the end.
        // The line that says which test failed is shown before the suite finishes.
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        let started = std::time::Instant::now();
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return Ok(ToolOutput::err(format!("spawn failed: {e}"))),
        };
        let mut group = Group::of(&child);
        if a.background {
            let id =
                crate::tools::background::start(a.cmd.clone(), child, group, ctx.max_output_chars);
            return Ok(ToolOutput::ok(format!("started process {id}\n{}", a.cmd)));
        }
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
        group.ended();
        let code = match status {
            Ok(s) => s.code().unwrap_or(-1),
            Err(e) => return Ok(ToolOutput::err(format!("could not wait for it: {e}"))),
        };

        crate::sound::command_ended(
            started.elapsed(),
            match code {
                0 => crate::sound::Outcome::Success,
                _ => crate::sound::Outcome::Error,
            },
        );
        let (out, err) = ends_of_streams(ctx, out, err);
        let mut report = String::new();
        report.push_str(&format!("exit: {code}\n"));
        report.push_str("stdout:\n");
        report.push_str(&out);
        report.push_str("\nstderr:\n");
        report.push_str(&err);
        if let Some(sandbox) = sandbox.filter(|_| code != 0) {
            report.push_str(&format!("\n{}", sandbox.note()));
            for line in _held.turned_away() {
                report.push_str(&format!("\nnetwork refused {line}"));
            }
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

    /// Output longer than the step's share keeps the status, the first lines and the last
    /// lines of each stream.
    #[tokio::test]
    async fn long_output_keeps_its_status_and_its_ends() {
        let mut h = Harness::new(Answer::Yes);
        h.ctx.step_share_chars = 2000;
        let out = h
            .run(
                &Bash,
                json!({"cmd": "seq 1 20000; echo 'error: the last word' >&2; exit 2"}),
            )
            .await;
        succeeded(&out);
        assert!(out.content.len() < 2400, "{}", out.content.len());
        assert!(
            out.content.starts_with("exit: 2\nstdout:\n1\n2\n"),
            "{}",
            out.content
        );
        assert!(out.content.contains("\n20000\n"), "{}", out.content);
        assert!(out.content.contains("of 20000 left out"), "{}", out.content);
        assert!(
            out.content.contains("error: the last word"),
            "{}",
            out.content
        );
    }

    /// A command that outlives its time is stopped with what it started.
    /// A command is the first process the system kills when memory runs out, not the session.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_command_is_killed_before_the_session_when_memory_runs_out() {
        let mut cmd = tokio::process::Command::new("cat");
        cmd.arg("/proc/self/oom_score_adj");
        own_group(&mut cmd);
        let out = cmd.output().await.unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "1000");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_command_that_times_out_is_stopped_with_its_children() {
        let h = Harness::new(Answer::Yes);
        let out = h
            .run(
                &Bash,
                json!({"cmd": "sleep 30 & echo $! > child.pid; wait", "timeout_s": 1}),
            )
            .await;
        assert!(out.is_error, "{out:?}");
        let pid: i32 = std::fs::read_to_string(h.root.join("child.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        // SAFETY: signal 0 only asks whether the process is there.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        assert!(!alive, "the command's child {pid} outlived it");
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

    /// A harness whose commands reach the network only through the proxy.
    ///
    /// `None` where this machine cannot make the namespaces, which the tests then skip.
    #[cfg(target_os = "linux")]
    async fn proxied() -> Option<Harness> {
        if !crate::sandbox::confine::available() {
            return None;
        }
        let mut h = Harness::confined();
        let allow = std::sync::Arc::new(["127.0.0.1".to_string()].into_iter().collect());
        let egress = crate::sandbox::egress::Egress::start(allow, Default::default())
            .await
            .ok()?;
        h.ctx.sandbox = Some(std::sync::Arc::new(crate::sandbox::confine::Sandbox {
            network: crate::sandbox::confine::Network::Proxy,
            egress: Some(std::sync::Arc::new(egress)),
            ..(**h.ctx.sandbox.as_ref().unwrap()).clone()
        }));
        Some(h)
    }

    /// The command asks the proxy for a tunnel to `port` and prints the answer line.
    #[cfg(target_os = "linux")]
    fn tunnel(port: u16) -> String {
        format!(
            "url=$https_proxy; hostport=${{url#*@}}; proxy=${{hostport##*:}}; \
             user=${{url#http://}}; user=${{user%%:*}}; \
             auth=$(printf '%s:x' \"$user\" | base64); \
             exec 3<>/dev/tcp/127.0.0.1/$proxy; \
             printf 'CONNECT 127.0.0.1:{port} HTTP/1.1\\r\\nProxy-Authorization: Basic %s\\r\\n\\r\\n' \"$auth\" >&3; \
             head -1 <&3 | tee /dev/stderr | grep -q ' 200 '"
        )
    }

    /// A service on this machine's loopback.
    #[cfg(target_os = "linux")]
    fn service() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for _ in 0..4 {
                let _ = listener.accept();
            }
        });
        port
    }

    /// A command in the sandbox reaches a host through the proxy once the host is agreed to,
    /// and the question names the command.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_command_reaches_an_agreed_host_through_the_proxy() {
        let Some(h) = proxied().await else {
            return;
        };
        h.write("tunnel.sh", &tunnel(service()));
        let out = h.run(&Bash, json!({"cmd": "bash tunnel.sh"})).await;
        succeeded(&out);
        assert!(out.content.contains("exit: 0"), "{}", out.content);
        let asked = h.asked();
        let host = asked
            .iter()
            .find(|r| matches!(&r.scope, ApprovalScope::Host(h) if h.starts_with("127.0.0.1:")))
            .expect("the host was not asked about");
        assert!(
            host.summary.starts_with("bash: bash tunnel.sh"),
            "{}",
            host.summary
        );
    }

    /// A command that ignores every proxy variable is still carried, by the name it asked for.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_connection_that_ignores_the_proxy_is_asked_about_by_name() {
        let Some(h) = proxied().await else {
            return;
        };
        let egress = h.ctx.sandbox.as_ref().unwrap().egress.as_ref().unwrap();
        if !egress.redirects() {
            return;
        }
        let out = h
            .run(
                &Bash,
                json!({"cmd": "unset https_proxy http_proxy HTTPS_PROXY HTTP_PROXY ALL_PROXY all_proxy; \
                    getent hosts svc.test; \
                    bash -c 'exec 3<>/dev/tcp/svc.test/80; sleep 1' 2>&1; true"}),
            )
            .await;
        succeeded(&out);
        assert!(out.content.contains("198.18."), "{}", out.content);
        assert!(
            h.asked()
                .iter()
                .any(|r| r.scope == ApprovalScope::Host("svc.test:80".into())),
            "{:?}",
            h.asked().iter().map(|r| &r.scope).collect::<Vec<_>>()
        );
    }

    /// An audit lets the host through without a question, and remembers it as agreed to.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn an_audit_records_the_host_instead_of_asking() {
        let Some(mut h) = proxied().await else {
            return;
        };
        let sandbox = h.ctx.sandbox.clone().unwrap();
        h.ctx.sandbox = Some(std::sync::Arc::new(crate::sandbox::confine::Sandbox {
            network: crate::sandbox::confine::Network::Audit,
            ..(*sandbox).clone()
        }));
        h.write("tunnel.sh", &tunnel(service()));
        let out = h.run(&Bash, json!({"cmd": "bash tunnel.sh"})).await;
        assert!(out.content.contains("exit: 0"), "{}", out.content);
        assert!(
            !h.asked()
                .iter()
                .any(|r| matches!(r.scope, ApprovalScope::Host(_))),
            "a host was asked about"
        );
        let hosts = h.ctx.session_approvals.hosts().await;
        assert!(
            matches!(hosts.as_slice(), [one] if one.starts_with("127.0.0.1:")),
            "{hosts:?}"
        );
        // The session lists it for `/net save`, and nothing agrees to it for the next one.
        assert!(
            !h.ctx
                .session_approvals
                .is_always(&ApprovalScope::Host(hosts[0].clone()))
                .await
        );
    }

    /// A rule that refuses a host still refuses it in an audit.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn an_audit_keeps_the_rules_that_refuse() {
        let Some(mut h) = proxied().await else {
            return;
        };
        let sandbox = h.ctx.sandbox.clone().unwrap();
        h.ctx.sandbox = Some(std::sync::Arc::new(crate::sandbox::confine::Sandbox {
            network: crate::sandbox::confine::Network::Audit,
            ..(*sandbox).clone()
        }));
        h.ctx.policy = std::sync::Arc::new(crate::sandbox::policy::Policy {
            deny: crate::sandbox::policy::Rules {
                egress: vec!["127.0.0.1".into()],
                ..Default::default()
            },
            ..Default::default()
        });
        h.write("tunnel.sh", &tunnel(service()));
        let out = h.run(&Bash, json!({"cmd": "bash tunnel.sh"})).await;
        assert!(out.content.contains("exit: 1"), "{}", out.content);
        assert!(h.ctx.session_approvals.hosts().await.is_empty());
    }

    /// The sockets of the machine's services are out of reach, and the working directory is kept.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn the_sockets_of_the_machines_services_are_out_of_reach() {
        // A socket in a directory of the kind an agent of the session leaves in /tmp.
        let Ok(dir) = tempfile::Builder::new().prefix("ssh-").tempdir_in("/tmp") else {
            return;
        };
        let socket = dir.path().join("agent.1");
        let Ok(_listener) = std::os::unix::net::UnixListener::bind(&socket) else {
            return;
        };
        assert!(std::os::unix::net::UnixStream::connect(&socket).is_ok());
        let Some(h) = proxied().await else {
            return;
        };
        let out = h
            .run(
                &Bash,
                json!({"cmd": format!(
                    "pwd; test -S {} || echo hidden; test -z \"$(ls -A /run)\" && echo run-empty",
                    socket.display()
                )}),
            )
            .await;
        assert!(
            out.content.contains(&h.root.display().to_string()),
            "{}",
            out.content
        );
        assert!(out.content.contains("hidden"), "{}", out.content);
        assert!(out.content.contains("run-empty"), "{}", out.content);
    }

    /// A rule that refuses the host is named to the model, which cannot see what the proxy said.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_refused_host_is_named_in_what_the_command_returns() {
        let Some(mut h) = proxied().await else {
            return;
        };
        h.ctx.policy = std::sync::Arc::new(crate::sandbox::policy::Policy {
            deny: crate::sandbox::policy::Rules {
                egress: vec!["127.0.0.1".into()],
                ..Default::default()
            },
            ..Default::default()
        });
        h.write("tunnel.sh", &tunnel(service()));
        let out = h.run(&Bash, json!({"cmd": "bash tunnel.sh"})).await;
        assert!(out.content.contains("exit: 1"), "{}", out.content);
        assert!(
            out.content
                .contains("network refused 127.0.0.1: refused by the policy rule"),
            "{}",
            out.content
        );
    }
}

/// Start the command as the leader of a process group of its own.
/// What it starts joins that group, so stopping the group stops all of it.
///
/// On Linux the command, and what it starts, is also the first process the system kills when
/// memory runs out, so the session that started it outlives it.
pub(crate) fn own_group(cmd: &mut tokio::process::Command) {
    #[cfg(unix)]
    cmd.process_group(0);
    #[cfg(target_os = "linux")]
    // SAFETY: the closure runs in the child between fork and exec, and calls only open, write and
    // close, which are async-signal-safe, on constant data.
    unsafe {
        cmd.pre_exec(|| {
            // The kernel's largest adjustment: this process is killed before any other.
            const FIRST_TO_GO: &[u8] = b"1000";
            let fd = libc::open(c"/proc/self/oom_score_adj".as_ptr(), libc::O_WRONLY);
            if fd >= 0 {
                libc::write(fd, FIRST_TO_GO.as_ptr().cast(), FIRST_TO_GO.len());
                libc::close(fd);
            }
            Ok(())
        });
    }
    #[cfg(not(unix))]
    let _ = cmd;
}

/// The process group of a running command, killed whole when this is dropped before the
/// command ended.
///
/// A timeout, a cancel or an abandoned turn drops it while the command runs.
/// The processes the command started are killed with it, and none is left holding a lock.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) struct Group(Option<u32>);

impl Group {
    pub(crate) fn of(child: &tokio::process::Child) -> Self {
        Self(child.id())
    }

    /// The command ended by itself, and what it left running is its own business.
    pub(crate) fn ended(&mut self) {
        self.0 = None;
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.0 {
            // SAFETY: a plain signal to the group the command leads.
            unsafe {
                libc::killpg(pid as libc::pid_t, libc::SIGKILL);
            }
        }
    }
}

/// What a command printed, cut to the step's share of the window.
///
/// Each stream that does not fit keeps its first and last lines, where a command says what it
/// started and how it ended.
/// The streams share the bound: one shorter than half of it leaves the rest to the other.
///
/// Shared with the test runner.
pub(crate) fn ends_of_streams(ctx: &ToolCtx, out: String, err: String) -> (String, String) {
    const RERUN: &str = "run the command again through grep, head or tail to see them.";
    let share = ctx.step_share_chars;
    let half = share / 2;
    let (o, e) = (out.len(), err.len());
    let (o_limit, e_limit) = if o + e <= share {
        (o, e)
    } else if e <= half {
        (share - e, e)
    } else if o <= half {
        (o, share - o)
    } else {
        (half, share - half)
    };
    (
        crate::tools::text::ends_of(out, o_limit, RERUN),
        crate::tools::text::ends_of(err, e_limit, RERUN),
    )
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
