//! Commands run at the edges of a tool call and a turn.
//!
//! A project states how it wants work checked - a formatter, a linter, a policy script -
//! and until now that could only be advice in a document the model may or may not follow.
//! A hook makes it part of the loop: what a pre-tool hook refuses does not happen, and
//! what it prints is handed to the model, which is the only form of guidance an agent
//! cannot skip.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

/// How long a hook may take before it is abandoned.
///
/// A hook sits between the model and its tool, so a slow one is felt on every call. Long
/// enough for a formatter on a file, short enough that a wedged command cannot hold the
/// turn.
const TIMEOUT: Duration = Duration::from_secs(30);

/// What a hook had to say.
pub struct Ran {
    /// False when the command refused: a non-zero exit.
    pub ok: bool,
    /// What it printed, both streams, trimmed and bounded.
    pub output: String,
}

/// Run `command` with the tool's name and the project in its environment.
///
/// A hook that cannot be started is reported as a refusal for a pre-tool hook and as
/// nothing at all for the others: refusing to run a tool because a hook is missing is the
/// safe reading, and refusing to end a turn over it is not.
pub async fn run(command: &str, root: &Path, tool: &str, when: &str) -> Ran {
    let started = std::time::Instant::now();
    let child = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .current_dir(root)
        .env("LOKODE_HOOK", when)
        .env("LOKODE_TOOL", tool)
        .env("LOKODE_ROOT", root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output();

    let out = match tokio::time::timeout(TIMEOUT, child).await {
        Err(_) => {
            tracing::warn!(hook = when, %tool, "hook timed out");
            return Ran {
                ok: false,
                output: format!("the {when} hook took longer than {}s", TIMEOUT.as_secs()),
            };
        }
        Ok(Err(e)) => {
            tracing::warn!(hook = when, %tool, why = %e, "hook could not be started");
            return Ran {
                ok: false,
                output: format!("the {when} hook could not be started: {e}"),
            };
        }
        Ok(Ok(out)) => out,
    };

    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    // A hook's output lands in the conversation, so it is bounded here rather than left
    // to fill the window with a build log.
    let mut text = text.trim().to_string();
    if text.len() > 4000 {
        let cut = (0..=4000)
            .rev()
            .find(|&i| text.is_char_boundary(i))
            .unwrap_or(0);
        text.truncate(cut);
        text.push_str("\n… (the rest of the hook's output was dropped)");
    }
    tracing::info!(
        hook = when,
        %tool,
        ok = out.status.success(),
        ms = started.elapsed().as_millis(),
        "hook finished"
    );
    Ran {
        ok: out.status.success(),
        output: text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_hook_that_succeeds_hands_back_what_it_printed() {
        let dir = tempfile::tempdir().unwrap();
        let ran = run(
            "echo checked $LOKODE_TOOL",
            dir.path(),
            "write_file",
            "pre-tool",
        )
        .await;
        assert!(ran.ok);
        assert_eq!(ran.output, "checked write_file");
    }

    /// The point of the pre-tool hook: a non-zero exit is a refusal, and what it printed
    /// is what the model is told.
    #[tokio::test]
    async fn a_hook_that_fails_says_so_and_keeps_its_output() {
        let dir = tempfile::tempdir().unwrap();
        let ran = run(
            "echo not on this branch >&2; exit 1",
            dir.path(),
            "bash",
            "pre-tool",
        )
        .await;
        assert!(!ran.ok);
        assert_eq!(ran.output, "not on this branch");
    }

    #[tokio::test]
    async fn a_hook_that_cannot_run_is_a_refusal_not_a_crash() {
        let dir = tempfile::tempdir().unwrap();
        let ran = run("exit 127", dir.path(), "bash", "pre-tool").await;
        assert!(!ran.ok);
    }
}
