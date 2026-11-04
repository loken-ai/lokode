//! Commands run at the edges of a session, a turn and a tool call.
//!
//! A project states how it wants work checked: a formatter, a linter, a policy script.
//! A hook makes it part of the loop.
//! What a pre-tool hook refuses does not happen.
//! What a hook says is handed to the model, the only form of guidance an agent cannot skip.
//!
//! A hook is given what happened as one JSON object on its standard input.
//! The same facts are in `LOKODE_HOOK`, `LOKODE_TOOL` and `LOKODE_ROOT`.
//! It answers by its exit code and what it prints:
//!
//! | Exit | Meaning |
//! |---|---|
//! | 0 | go on. What it printed is handed on, or read as a decision when it is a JSON object |
//! | 2 | block. What it printed to its error output, or else to its output, says why |
//! | other | it failed. A pre-tool hook that fails blocks the call |
//!
//! A decision is `{"decision": "block", "reason": "..."}`.
//! It may add `context` for text to hand the model.
//! Before a tool, it may add `args` for the arguments to call it with instead.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::config::Hook;

/// How long a hook may take before it is stopped, when it does not say.
///
/// A hook sits between the model and its tool, so a slow one is felt on every call.
/// The limit stops a wedged command from holding the turn.
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// How much of a hook's output reaches the conversation.
const OUTPUT_CHARS: usize = 4000;

/// When a hook runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// Before the first turn of a session. What it says goes with the first question.
    SessionStart,
    /// When a question is asked, before the model sees it. It may refuse the question.
    Prompt,
    /// Before a tool runs. It may refuse the call or change its arguments.
    PreTool,
    /// After a tool ran. What it says goes with the result.
    PostTool,
    /// When the model has answered. Blocking hands the reason back and the turn goes on.
    TurnEnd,
    /// When an agent a turn handed work to has answered. What it says goes with the answer.
    AgentEnd,
    /// When the oldest steps of the session have left the window.
    Compact,
    /// When the session ends.
    SessionEnd,
    /// When the session waits for the person: a question to answer, a change to approve.
    Notification,
}

impl Event {
    pub fn label(self) -> &'static str {
        match self {
            Event::SessionStart => "session-start",
            Event::Prompt => "prompt",
            Event::PreTool => "pre-tool",
            Event::PostTool => "post-tool",
            Event::TurnEnd => "turn-end",
            Event::AgentEnd => "agent-end",
            Event::Compact => "compact",
            Event::SessionEnd => "session-end",
            Event::Notification => "notification",
        }
    }
}

/// What one hook had to say.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Ran {
    /// It blocked: exit 2, a decision to block, or a pre-tool hook that failed.
    pub blocked: bool,
    /// It failed without blocking: a non-zero exit other than 2.
    pub failed: bool,
    /// What it said, bounded: the reason, the context, or what it printed.
    pub output: String,
    /// The arguments a pre-tool hook wants the call made with instead.
    pub args: Option<Value>,
}

/// Whether `hook` runs for `tool`.
///
/// A hook that names tools with `match` runs for those, written as patterns joined by `|`.
/// One that names none runs for the tools that change something.
/// Away from a tool, every hook runs.
pub fn applies(hook: &Hook, tool: Option<(&str, bool)>) -> bool {
    match (tool, &hook.matches) {
        (None, _) => true,
        (Some((_, mutates)), None) => mutates,
        (Some((name, _)), Some(patterns)) => patterns
            .split('|')
            .any(|p| crate::sandbox::policy::glob_matches(p.trim(), name)),
    }
}

/// Tell the hooks of `event` that the session waits for the person.
///
/// The turn goes on without waiting for them.
pub fn notify(hooks: &std::sync::Arc<Vec<Hook>>, root: &Path, message: &str) {
    if hooks.is_empty() {
        return;
    }
    let (hooks, root) = (hooks.clone(), root.to_path_buf());
    let input = serde_json::json!({
        "event": Event::Notification.label(),
        "root": root,
        "message": message,
        "hook_event_name": "Notification",
    });
    tokio::spawn(async move {
        run_all(&hooks, Event::Notification, &root, None, input).await;
    });
}

/// Run each of `hooks` that applies, in order, and gather what they said.
///
/// A pre-tool hook's new arguments are what the next one is given.
pub async fn run_all(
    hooks: &[Hook],
    event: Event,
    root: &Path,
    tool: Option<(&str, bool)>,
    mut input: Value,
) -> Vec<Ran> {
    let mut out = Vec::new();
    for hook in hooks.iter().filter(|h| applies(h, tool)) {
        let ran = run(
            hook,
            event,
            root,
            tool.map(|(n, _)| n).unwrap_or(""),
            &input,
        )
        .await;
        if let (Some(args), Some(slot)) = (&ran.args, input.get_mut("args")) {
            *slot = args.clone();
        }
        let blocked = ran.blocked;
        out.push(ran);
        if blocked {
            break;
        }
    }
    out
}

/// Run one hook with `input` on its standard input.
pub async fn run(hook: &Hook, event: Event, root: &Path, tool: &str, input: &Value) -> Ran {
    let when = event.label();
    let started = std::time::Instant::now();
    let timeout = hook.timeout.map(Duration::from_secs).unwrap_or(TIMEOUT);
    let shell = crate::shell::Shell::current();
    let mut cmd = Command::new(&shell.program);
    cmd.args(shell.args(&hook.command, false))
        .current_dir(root)
        .env("LOKODE_HOOK", when)
        .env("LOKODE_TOOL", tool)
        .env("LOKODE_ROOT", root)
        // What the hooks of other tools' plugins name the project by.
        .env("CLAUDE_PROJECT_DIR", root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // A group of its own, so stopping it stops what it started too.
    #[cfg(unix)]
    cmd.process_group(0);
    let failed = |output: String| Ran {
        blocked: event == Event::PreTool,
        failed: true,
        output,
        args: None,
    };
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(hook = when, %tool, why = %e, "hook could not be started");
            return failed(format!("the {when} hook could not be started: {e}"));
        }
    };
    #[cfg_attr(not(unix), allow(unused_variables))]
    let group = child.id();
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input.to_string().as_bytes()).await;
    }
    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return failed(format!("the {when} hook could not be waited for: {e}")),
        Err(_) => {
            #[cfg(unix)]
            if let Some(pid) = group {
                // SAFETY: a plain signal to the group the hook leads.
                unsafe {
                    libc::killpg(pid as libc::pid_t, libc::SIGKILL);
                }
            }
            tracing::warn!(hook = when, %tool, "hook timed out");
            return failed(format!(
                "the {when} hook took longer than {}s",
                timeout.as_secs()
            ));
        }
    };
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let code = out.status.code();
    tracing::info!(hook = when, %tool, ?code, ms = started.elapsed().as_millis(), "hook finished");
    let decision = serde_json::from_str::<Value>(&stdout)
        .ok()
        .filter(Value::is_object);
    let ran = match (code, decision) {
        (Some(2), _) => Ran {
            blocked: true,
            output: if stderr.is_empty() { stdout } else { stderr },
            ..Default::default()
        },
        (Some(0), Some(d)) => {
            let text = |key: &str| d.get(key).and_then(|v| v.as_str()).map(str::to_string);
            let said: Vec<String> = [text("reason"), text("context")]
                .into_iter()
                .flatten()
                .filter(|t| !t.trim().is_empty())
                .collect();
            // A refusal as other tools' hooks write it, beside lokode's.
            let denied = d
                .pointer("/hookSpecificOutput/permissionDecision")
                .and_then(|v| v.as_str())
                == Some("deny");
            let said: Vec<String> = match d
                .pointer("/hookSpecificOutput/permissionDecisionReason")
                .and_then(|v| v.as_str())
            {
                Some(why) if denied => vec![why.to_string()],
                _ => said,
            };
            Ran {
                blocked: denied || text("decision").as_deref() == Some("block"),
                failed: false,
                output: said.join("\n"),
                args: d.get("args").filter(|a| a.is_object()).cloned(),
            }
        }
        (Some(0), None) => Ran {
            output: [stdout, stderr]
                .into_iter()
                .filter(|t| !t.is_empty())
                .collect::<Vec<_>>()
                .join("\n"),
            ..Default::default()
        },
        _ => failed(
            [stdout, stderr]
                .into_iter()
                .filter(|t| !t.is_empty())
                .collect::<Vec<_>>()
                .join("\n"),
        ),
    };
    Ran {
        output: bounded(ran.output),
        ..ran
    }
}

/// A hook's output lands in the conversation, so it is bounded.
/// A build log does not fill the window.
fn bounded(mut text: String) -> String {
    if text.len() > OUTPUT_CHARS {
        let cut = (0..=OUTPUT_CHARS)
            .rev()
            .find(|&i| text.is_char_boundary(i))
            .unwrap_or(0);
        text.truncate(cut);
        text.push_str("\n… (the rest of the hook's output was dropped)");
    }
    text
}

/// What hooks said, as one block with a heading, or nothing when they said nothing.
pub fn said(event: Event, rans: &[Ran]) -> Option<String> {
    let text: Vec<&str> = rans
        .iter()
        .map(|r| r.output.as_str())
        .filter(|o| !o.is_empty())
        .collect();
    (!text.is_empty()).then(|| format!("{} hook:\n{}", event.label(), text.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hook(command: &str) -> Hook {
        Hook {
            command: command.into(),
            matches: None,
            timeout: None,
        }
    }

    async fn once(command: &str, event: Event) -> Ran {
        let dir = tempfile::tempdir().unwrap();
        run(
            &hook(command),
            event,
            dir.path(),
            "bash",
            &json!({"args": {"cmd": "ls"}}),
        )
        .await
    }

    #[tokio::test]
    async fn a_hook_that_succeeds_hands_back_what_it_printed() {
        let ran = once("echo checked $LOKODE_TOOL", Event::PostTool).await;
        assert!(!ran.blocked && !ran.failed);
        assert_eq!(ran.output, "checked bash");
    }

    /// What happened reaches the hook as JSON on its input.
    #[tokio::test]
    async fn a_hook_reads_what_happened() {
        let ran = once("grep -o '\"cmd\":\"ls\"'", Event::PreTool).await;
        assert_eq!(ran.output, "\"cmd\":\"ls\"");
    }

    /// Exit 2 blocks with what was said.
    /// Another failure blocks only a pre-tool hook.
    #[tokio::test]
    async fn exit_codes_say_what_a_hook_decided() {
        let ran = once("echo not on this branch >&2; exit 2", Event::TurnEnd).await;
        assert!(ran.blocked);
        assert_eq!(ran.output, "not on this branch");
        assert!(once("exit 1", Event::PreTool).await.blocked);
        let ran = once("exit 1", Event::PostTool).await;
        assert!(!ran.blocked && ran.failed);
        assert!(once("exit 127", Event::PreTool).await.blocked);
    }

    /// A decision printed as JSON is read: a block with its reason, context, new arguments.
    #[tokio::test]
    async fn a_decision_is_read() {
        let ran = once(
            r#"echo '{"decision":"block","reason":"use the test runner","args":{"cmd":"cargo test"}}'"#,
            Event::PreTool,
        )
        .await;
        assert!(ran.blocked);
        assert_eq!(ran.output, "use the test runner");
        assert_eq!(ran.args, Some(json!({"cmd": "cargo test"})));
    }

    /// A hook that names tools runs for those.
    /// One that names none runs for the tools that change something.
    /// Every hook runs away from a tool.
    #[test]
    fn a_hook_runs_for_the_tools_it_names() {
        let any = hook("x");
        let reads = Hook {
            matches: Some("read_*|grep".into()),
            ..hook("x")
        };
        assert!(applies(&any, Some(("bash", true))));
        assert!(!applies(&any, Some(("read_file", false))));
        assert!(applies(&reads, Some(("read_file", false))));
        assert!(applies(&reads, Some(("grep", false))));
        assert!(!applies(&reads, Some(("bash", true))));
        assert!(applies(&reads, None));
    }

    /// A hook that outlives its time is stopped, with what it started.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_hook_that_hangs_is_stopped_with_its_children() {
        let dir = tempfile::tempdir().unwrap();
        let slow = Hook {
            timeout: Some(1),
            ..hook("sleep 30 & echo $! > child.pid; wait")
        };
        let ran = run(&slow, Event::TurnEnd, dir.path(), "", &json!({})).await;
        assert!(ran.failed, "{ran:?}");
        let pid: i32 = std::fs::read_to_string(dir.path().join("child.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        // SAFETY: signal 0 only asks whether the process is there.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        assert!(!alive, "the hook's child {pid} outlived it");
    }
}
