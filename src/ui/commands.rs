//! The slash commands, and the things a typed line can ask of the session itself.

use tokio::sync::mpsc;

use crate::checkpoint::Checkpoints;
use crate::ui::chat::Entry;
use crate::ui::state::UiState;

/// Put a turn back, or offer the turns that can be.
///
/// Asked rather than done: undoing used to take the most recent turn without saying which
/// it was or what it held, which is a step into the dark on the one command whose whole
/// purpose is to recover from one.
pub(crate) async fn undo_command(checkpoints: Option<&Checkpoints>, rest: &str) -> Ran {
    let Some(cp) = checkpoints else {
        return Ran::Said("no change history for this session".into());
    };
    if let Ok(turn) = rest.trim().parse::<u32>() {
        return Ran::Said(match cp.undo_turn(turn).await {
            Err(e) => format!("undo failed: {e:#}"),
            Ok(None) => format!("turn {turn} is not one that can be put back"),
            Ok(Some(done)) => match done.files.is_empty() {
                true => format!("turn {} put back", done.turn),
                false => format!(
                    "turn {} put back, {} file(s):\n{}",
                    done.turn,
                    done.files.len(),
                    done.files.join("\n")
                ),
            },
        });
    }
    let turns = match cp.turns().await {
        Err(e) => return Ran::Said(format!("undo failed: {e:#}")),
        Ok(t) => t,
    };
    if turns.is_empty() {
        return Ran::Said("nothing to undo: the agent has not changed anything".into());
    }
    Ran::Choose(Box::new(crate::ui::chooser::Chooser::new(
        "undo",
        "put the project back to before which turn",
        turns
            .into_iter()
            .map(|t| {
                crate::ui::chooser::Choice::new(
                    t.turn.to_string(),
                    format!("turn {}: {}", t.turn, t.files.join(", ")),
                )
            })
            .collect(),
        None,
    )))
}

/// How long ago something was, in the words a person uses for it.
pub(crate) fn how_long_ago(when: u64) -> String {
    let now = crate::agent::transcript::now_secs();
    let seconds = now.saturating_sub(when);
    match seconds {
        0..=90 => "just now".to_string(),
        91..=5400 => format!("{} min ago", seconds / 60),
        5401..=172800 => format!("{} hour(s) ago", seconds / 3600),
        _ => format!("{} day(s) ago", seconds / 86400),
    }
}

/// Add one rule to what this project tells the agent, and say where it went.
///
/// Appended rather than rewritten: the file is the team's, it is in version control with
/// the code it is about, and a tool that reorganises it would be a tool nobody lets near
/// it twice.
pub(crate) fn remember_rule(rule: &str, root: &std::path::Path) -> String {
    use std::io::Write;
    let path = crate::config::project_instructions_path(root);
    let fresh = !path.exists();
    let opened = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path);
    let mut file = match opened {
        Ok(f) => f,
        Err(e) => return format!("could not write {}: {e}", path.display()),
    };
    // A heading only where the file is being started, so an existing one keeps whatever
    // shape its authors gave it.
    let text = match fresh {
        true => format!("# What this project expects\n\n- {rule}\n"),
        false => format!("- {rule}\n"),
    };
    match file.write_all(text.as_bytes()) {
        Ok(()) => format!("written to {}", path.display()),
        Err(e) => format!("could not write {}: {e}", path.display()),
    }
}

/// Take the last question out of the conversation and give it back as text.
///
/// The conversation is rewound past it, so what the agent did in answer leaves with it: a
/// second attempt that can still see the first one is a model reading its own failure
/// while it tries again.
pub(crate) async fn take_back(
    control: &mpsc::Sender<crate::agent::agent_loop::Control>,
) -> Option<String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    control
        .send(crate::agent::agent_loop::Control::Retry {
            model: None,
            reply: tx,
        })
        .await
        .ok()?;
    rx.await.ok()?.ok()
}

/// Run one command the person typed, and give back what it said.
///
/// Theirs rather than the model's, so it is not approved and not confined: they typed it,
/// which is the same decision they make at a shell prompt. It still goes through the
/// project root, because that is where the question they are asking lives.
///
/// Bounded like any other result that lands in the pane, and the two streams are kept in
/// the order the command wrote them as far as reading them separately allows: what a
/// failing command prints is usually the only thing worth having.
pub(crate) async fn run_shell(
    command: &str,
    root: &std::path::Path,
    cap: usize,
    stop: tokio_util::sync::CancellationToken,
    lines: impl Fn(String),
) -> (bool, String) {
    use tokio::io::AsyncBufReadExt;
    // Killed when it is stopped or dropped, rather than left running unseen.
    let child = tokio::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(e) => return (true, format!("could not run it: {e}")),
    };
    let mut out = child
        .stdout
        .take()
        .map(|o| tokio::io::BufReader::new(o).lines());
    let mut err = child
        .stderr
        .take()
        .map(|e| tokio::io::BufReader::new(e).lines());
    // Both streams in the order their lines arrive, as a terminal shows them.
    let mut body = String::new();
    while out.is_some() || err.is_some() {
        let line = tokio::select! {
            _ = stop.cancelled() => return (true, "stopped before it finished".into()),
            line = async { out.as_mut()?.next_line().await.ok().flatten() }, if out.is_some() => {
                if line.is_none() {
                    out = None;
                }
                line
            }
            line = async { err.as_mut()?.next_line().await.ok().flatten() }, if err.is_some() => {
                if line.is_none() {
                    err = None;
                }
                line
            }
        };
        if let Some(line) = line {
            if body.len() <= cap {
                body.push_str(&line);
                body.push('\n');
            }
            lines(line);
        }
    }
    let status = tokio::select! {
        _ = stop.cancelled() => return (true, "stopped before it finished".into()),
        status = child.wait() => status,
    };
    let ok = status.as_ref().is_ok_and(|s| s.success());
    let body = body.trim_end().to_string();
    let mut body = match (body.is_empty(), &status) {
        (false, _) => body,
        (true, _) if ok => "(said nothing)".into(),
        (true, Ok(s)) => format!("(said nothing, {s})"),
        (true, Err(e)) => format!("(said nothing, {e})"),
    };
    if body.len() > cap {
        let cut = (0..=cap)
            .rev()
            .find(|&i| body.is_char_boundary(i))
            .unwrap_or(0);
        body.truncate(cut);
        body.push_str("\n… (truncated to fit the pane)");
    }
    (!ok, body)
}

/// Put the conversation where a person can take it: on the terminal's clipboard, and in a
/// file that is always there whether or not the terminal took it.
///
/// Both, because neither can be relied on alone. A terminal either understands the copy
/// sequence or ignores it without saying so, and a file is no use to somebody who wanted
/// to paste into a message.
pub(crate) fn copy_out(entries: &[Entry], last_only: bool, root: &std::path::Path) -> String {
    let text = match last_only {
        true => entries
            .iter()
            .rev()
            .find(|e| matches!(e, Entry::Assistant(_) | Entry::AssistantStreaming(_)))
            .map(|e| e.to_text())
            .unwrap_or_default(),
        false => entries
            .iter()
            .map(|e| e.to_text())
            .collect::<Vec<_>>()
            .join("\n\n"),
    };
    if text.trim().is_empty() {
        return "nothing to copy yet".to_string();
    }
    let mut said = Vec::new();
    if text.len() <= crate::ui::clipboard::MOST && crate::ui::clipboard::put(&text).is_ok() {
        said.push("sent to the terminal's clipboard".to_string());
    }
    let path = crate::paths::state_dir()
        .unwrap_or_else(|| root.to_path_buf())
        .join("last-copy.txt");
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match std::fs::write(&path, &text) {
        Ok(()) => said.push(format!("written to {}", path.display())),
        Err(e) => said.push(format!("could not be written ({e})")),
    }
    format!("{} line(s): {}", text.lines().count(), said.join(", "))
}

/// Ask the agent what it is on and how full the window is.
/// Both move under the interface, and both are drawn every frame, so they are read back
/// at the moments they can have changed rather than watched.
pub(crate) async fn refresh_status(
    control: &mpsc::Sender<crate::agent::agent_loop::Control>,
    checkpoints: Option<&Checkpoints>,
    state: &mut UiState,
) {
    state.git = git_state(&state.root).await;
    // What the change history holds, for the rail: the turns that can be put back and
    // what the session changed, file by file.
    if let Some(cp) = checkpoints {
        state.turns = cp.turns().await.unwrap_or_default();
        state.changes = cp
            .diff()
            .await
            .map(|d| crate::ui::diff::changes(&d))
            .unwrap_or_default();
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    if control
        .send(crate::agent::agent_loop::Control::Status { reply: tx })
        .await
        .is_err()
    {
        return;
    }
    if let Ok(report) = rx.await {
        state.model = report.model;
        state.window = report.window;
        state.protocol = report.protocol;
    }
}

/// The branch the project is on and how many files differ from it, or nothing when the
/// project is not a git repository.
pub(crate) async fn git_state(root: &std::path::Path) -> Option<(String, usize)> {
    let git = |args: &'static [&'static str]| {
        tokio::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
    };
    let branch = git(&["rev-parse", "--abbrev-ref", "HEAD"]).await.ok()?;
    if !branch.status.success() {
        return None;
    }
    let status = git(&["status", "--porcelain"]).await.ok()?;
    Some((
        String::from_utf8_lossy(&branch.stdout).trim().to_string(),
        String::from_utf8_lossy(&status.stdout).lines().count(),
    ))
}

/// What the servers are and what they offer, for `/mcp`.
pub(crate) fn booted_summary(mcp: &crate::mcp::Loaded) -> String {
    if mcp.clients.is_empty() {
        return "no tool servers are configured".to_string();
    }
    let mut out = String::from("## Servers\n\n");
    for c in &mcp.clients {
        out.push_str(&format!("- **{}** running\n", c.server));
    }
    if !mcp.prompts.is_empty() {
        out.push_str("\n## Prompts\n\n| Prompt | What it does |\n|---|---|\n");
        for (server, prompt, about) in &mcp.prompts {
            out.push_str(&format!(
                "| `/mcp {server} {prompt}` | {} |\n",
                about.replace('|', "/")
            ));
        }
    }
    out
}

/// What running a command came to.
pub(crate) enum Ran {
    /// Something to put on screen.
    Said(String),
    /// Instructions to hand the model as this turn's prompt.
    Prompt(String),
    /// A list to choose from, and the command to run with what is chosen.
    Choose(Box<crate::ui::chooser::Chooser>),
    /// The conversation is over and the screen goes with it.
    Cleared(String),
    /// Another session is now the one being had, and the screen shows it.
    Resumed(Box<crate::agent::agent_loop::Resumed>),
}

/// Everything a command may need. One struct rather than nine arguments, since every new
/// command was adding another to a call nobody could read.
pub(crate) struct CommandCtx<'a> {
    pub(crate) checkpoints: Option<&'a Checkpoints>,
    pub(crate) root: &'a std::path::Path,
    pub(crate) control: &'a mpsc::Sender<crate::agent::agent_loop::Control>,
    pub(crate) policy: &'a crate::sandbox::policy::Policy,
    pub(crate) tools: &'a [String],
    /// What is known about the project, for /digest.
    pub(crate) digest: &'a tokio::sync::Mutex<crate::agent::digest::Digest>,
    pub(crate) mcp: &'a [std::sync::Arc<crate::mcp::client::McpClient>],
    pub(crate) mcp_summary: &'a str,
    /// The session being had now, so coming back to a session does not offer this one.
    pub(crate) session_id: &'a str,
    /// What has been agreed to here, which is the half of the policy that can change.
    pub(crate) approvals: &'a crate::sandbox::approval::SessionApprovals,
    /// A control request is answered between turns, so a command that needs one has to
    /// wait rather than sit silent behind a running turn.
    pub(crate) busy: bool,
}

/// Answer a slash command.
///
/// These do not reach the model and do not become part of the conversation: they are things
/// the user is asking of the tool about the session, and putting them in the transcript
/// would describe a turn that never happened.
pub(crate) async fn run_command(command: &str, cx: CommandCtx<'_>) -> Ran {
    let CommandCtx {
        checkpoints,
        root,
        control,
        policy,
        tools,
        digest,
        mcp,
        mcp_summary,
        session_id,
        approvals,
        busy,
    } = cx;
    let (verb, rest) = match command.split_once(char::is_whitespace) {
        Some((v, r)) => (v, r.trim()),
        None => (command, ""),
    };
    // A project's own commands, before the built-ins are consulted so a team can name
    // one of theirs after a built-in only by choosing to.
    if let Some(text) = crate::config::user_command(root, verb) {
        // What follows the command name is handed to it, which is how one command serves
        // a family of tasks. A command that names no argument still gets it, appended,
        // so nothing a user typed is silently dropped.
        let text = match (rest.is_empty(), text.contains("$ARGS")) {
            (true, _) => text.replace("$ARGS", ""),
            (false, true) => text.replace("$ARGS", rest),
            (false, false) => format!("{text}\n\n{rest}"),
        };
        return Ran::Prompt(text);
    }
    if verb == "digest" {
        // Asked first, because this throws away everything a session worked out about the
        // project and there is nothing to put it back with. A word typed by mistake is
        // the cheapest way there is to lose an afternoon of reading.
        if rest == "forget" {
            return Ran::Choose(Box::new(crate::ui::chooser::Chooser::new(
                "digest forget",
                "throw away what has been worked out about this project",
                vec![
                    crate::ui::chooser::Choice::new("no", "keep it"),
                    crate::ui::chooser::Choice::new("yes", "it will be worked out again"),
                ],
                Some("no".into()),
            )));
        }
        let mut d = digest.lock().await;
        if rest == "forget yes" {
            d.forget();
            return Ran::Said("forgotten; it will be worked out again".into());
        }
        return Ran::Said(d.describe());
    }
    if verb == "why" {
        if busy {
            return Ran::Said("the agent is working; ask again when the turn is over".into());
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        if control
            .send(crate::agent::agent_loop::Control::Why { reply: tx })
            .await
            .is_err()
        {
            return Ran::Said("the agent is not listening".into());
        }
        return Ran::Said(rx.await.unwrap_or_else(|_| "the agent said nothing".into()));
    }
    if verb == "cost" {
        if busy {
            return Ran::Said("the agent is working; ask again when the turn is over".into());
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        if control
            .send(crate::agent::agent_loop::Control::Spend { reply: tx })
            .await
            .is_err()
        {
            return Ran::Said("the agent is not listening".into());
        }
        return Ran::Said(rx.await.unwrap_or_else(|_| "the agent said nothing".into()));
    }
    if verb == "context" || verb == "compact" {
        if busy {
            return Ran::Said("the agent is working; ask again when the turn is over".into());
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        let request = if verb == "context" {
            crate::agent::agent_loop::Control::Context { reply: tx }
        } else {
            crate::agent::agent_loop::Control::Compact { reply: tx }
        };
        if control.send(request).await.is_err() {
            return Ran::Said("the agent is not listening".into());
        }
        return Ran::Said(rx.await.unwrap_or_else(|_| "the agent said nothing".into()));
    }
    if verb == "mcp" {
        // With a server and a prompt named, the prompt is fetched and becomes the turn.
        let mut words = rest.split_whitespace();
        match (words.next(), words.next()) {
            (Some(server), Some(prompt)) => {
                let Some(client) = mcp.iter().find(|c| c.server == server) else {
                    return Ran::Said(format!("no server called {server}"));
                };
                let args: serde_json::Value = serde_json::json!({});
                return match client.get_prompt(prompt, args).await {
                    Ok(text) => Ran::Prompt(text),
                    Err(e) => Ran::Said(format!("{server}/{prompt}: {e:#}")),
                };
            }
            // A server named and nothing else: what it offers, to be chosen from. Its
            // prompts are named by whoever wrote the server, and nobody keeps those in
            // their head either.
            (Some(server), None) => {
                let Some(client) = mcp.iter().find(|c| c.server == server) else {
                    return Ran::Said(format!("no server called {server}"));
                };
                let prompts = client.list_prompts().await;
                if prompts.is_empty() {
                    return Ran::Said(format!("{server} offers no prompt"));
                }
                return Ran::Choose(Box::new(crate::ui::chooser::Chooser::new(
                    format!("mcp {server}"),
                    format!("what to take from {server}"),
                    prompts
                        .into_iter()
                        .map(|(name, about)| crate::ui::chooser::Choice::new(name, about))
                        .collect(),
                    None,
                )));
            }
            _ => return Ran::Said(mcp_summary.to_string()),
        }
    }
    if verb == "resume" {
        if busy {
            return Ran::Said("the agent is working; ask again when the turn is over".into());
        }
        let Some(dir) = crate::paths::sessions_dir() else {
            return Ran::Said("no home directory, so no sessions to come back to".into());
        };
        if rest.is_empty() {
            let recorded: Vec<_> = crate::agent::transcript::recorded_for_root(&dir, root)
                .into_iter()
                .filter(|r| r.id != session_id)
                .collect();
            if recorded.is_empty() {
                return Ran::Said("no other session has been recorded for this project".into());
            }
            return Ran::Choose(Box::new(crate::ui::chooser::Chooser::new(
                "resume",
                "come back to which session",
                recorded
                    .into_iter()
                    .map(|r| {
                        crate::ui::chooser::Choice::new(
                            r.id,
                            format!(
                                "{}  {} message(s)  {}",
                                how_long_ago(r.when),
                                r.messages,
                                r.opening
                            ),
                        )
                    })
                    .collect(),
                None,
            )));
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        if control
            .send(crate::agent::agent_loop::Control::Resume {
                id: rest.to_string(),
                reply: tx,
            })
            .await
            .is_err()
        {
            return Ran::Said("the agent is not listening".into());
        }
        return match rx.await {
            Err(_) => Ran::Said("the agent said nothing".into()),
            Ok(Err(why)) => Ran::Said(why),
            Ok(Ok(resumed)) => Ran::Resumed(Box::new(resumed)),
        };
    }
    if verb == "clear" {
        if busy {
            return Ran::Said("the agent is working; ask again when the turn is over".into());
        }
        // Asked first: what was said leaves the window and does not come back.
        if rest != "yes" {
            return Ran::Choose(Box::new(crate::ui::chooser::Chooser::new(
                "clear",
                "start the conversation again? what was said leaves the window",
                vec![
                    crate::ui::chooser::Choice::new("no", "keep the conversation"),
                    crate::ui::chooser::Choice::new("yes", "start again"),
                ],
                Some("no".into()),
            )));
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        if control
            .send(crate::agent::agent_loop::Control::Clear { reply: tx })
            .await
            .is_err()
        {
            return Ran::Said("the agent is not listening".into());
        }
        return Ran::Cleared(rx.await.unwrap_or_else(|_| "the agent said nothing".into()));
    }
    if verb == "tools" {
        let mut out = String::from("| Tool | From | What it does |\n|---|---|---|\n");
        for row in tools {
            let (head, about) = row.split_once(": ").unwrap_or((row.as_str(), ""));
            let (name, from) = match head.split_once(" (from ") {
                Some((name, server)) => (name, server.trim_end_matches(')')),
                None => (head, "lokode"),
            };
            let first = about.split(". ").next().unwrap_or(about).replace('|', "/");
            out.push_str(&format!(
                "| `{name}` | {from} | {} |\n",
                first.trim_end_matches('.')
            ));
        }
        return Ran::Said(out);
    }
    if verb == "approve" {
        // What is in force, which is two things: what the configuration settles without
        // asking, and what was agreed to here. The second used to be invisible - a
        // session could be running commands unasked because of a key pressed an hour ago,
        // with nothing anywhere to say so and no way to take it back.
        if rest == "forget" {
            let had = approvals.forget_always().await;
            return Ran::Said(match had {
                0 => "nothing had been agreed to this session".to_string(),
                n => format!("{n} standing agreement(s) taken back; they will be asked again"),
            });
        }
        let standing = approvals.standing(root).await;
        let mut out = String::from("## Settled in the configuration\n\n");
        for line in policy.describe().lines() {
            out.push_str(&match line.split_once(": ") {
                Some((rule, list)) => format!(
                    "- **{rule}**: {}\n",
                    list.split(", ")
                        .map(|p| format!("`{p}`"))
                        .collect::<Vec<_>>()
                        .join(" ")
                ),
                None => format!("{line}\n"),
            });
        }
        out.push_str("\n## Agreed for this session\n\n");
        match standing.is_empty() {
            true => out.push_str("Nothing has been agreed to for the rest of this session.\n"),
            false => {
                for s in &standing {
                    out.push_str(&format!("- {s}\n"));
                }
                out.push_str("\n`/approve forget` takes them back.\n");
            }
        }
        return Ran::Said(out);
    }
    if verb == "plan" || verb == "act" {
        if busy {
            return Ran::Said("the agent is working; ask again when the turn is over".into());
        }
        let mode = if verb == "plan" {
            crate::agent::agent_loop::Mode::Plan
        } else {
            crate::agent::agent_loop::Mode::Act
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        if control
            .send(crate::agent::agent_loop::Control::Mode { mode, reply: tx })
            .await
            .is_err()
        {
            return Ran::Said("the agent is not listening".into());
        }
        return Ran::Said(rx.await.unwrap_or_else(|_| "the agent said nothing".into()));
    }
    if verb == "retry" {
        if busy {
            return Ran::Said("the agent is working; ask again when the turn is over".into());
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        let request = crate::agent::agent_loop::Control::Retry {
            model: (!rest.is_empty()).then(|| rest.to_string()),
            reply: tx,
        };
        if control.send(request).await.is_err() {
            return Ran::Said("the agent is not listening".into());
        }
        return match rx.await {
            Ok(Ok(asked)) => Ran::Prompt(asked),
            Ok(Err(why)) => Ran::Said(why),
            Err(_) => Ran::Said("the agent said nothing".into()),
        };
    }
    if verb == "model" {
        // The agent reads control requests between turns, so asking during one would
        // wait for the turn to end with nothing on screen explaining the silence.
        if busy {
            return Ran::Said("the agent is working; ask again when the turn is over".into());
        }
        // Named, it switches. Unnamed, it offers the list: printing the catalogue and
        // leaving the person to type a name back exactly is not a way to choose.
        if rest.is_empty() {
            let (tx, rx) = tokio::sync::oneshot::channel();
            if control
                .send(crate::agent::agent_loop::Control::Models { reply: tx })
                .await
                .is_err()
            {
                return Ran::Said("the agent is not listening".into());
            }
            return match rx.await {
                Err(_) => Ran::Said("the agent said nothing".into()),
                Ok(Err(why)) => Ran::Said(format!("{why}; name one with /model <name>")),
                Ok(Ok(models)) => Ran::Choose(Box::new(crate::ui::chooser::Chooser::new(
                    "model",
                    "which model answers",
                    models
                        .all
                        .iter()
                        .map(|m| {
                            // What else a model is used for, beside its name: a session
                            // that plans with one model and edits with another has to be
                            // able to see which is which before it changes one of them.
                            let roles: Vec<&str> = models
                                .roles
                                .iter()
                                .filter(|(_, model)| model == m)
                                .map(|(role, _)| role.as_str())
                                .collect();
                            crate::ui::chooser::Choice::new(m, roles.join(", "))
                        })
                        .collect(),
                    Some(models.current),
                ))),
            };
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        let request = crate::agent::agent_loop::Control::Model {
            name: (!rest.is_empty()).then(|| rest.to_string()),
            reply: tx,
        };
        if control.send(request).await.is_err() {
            return Ran::Said("the agent is not listening".into());
        }
        return Ran::Said(rx.await.unwrap_or_else(|_| "the agent said nothing".into()));
    }
    // On the verb, not on the whole line: matching the line meant a command given an
    // argument was an unknown command, so anything typed after one of these got the help
    // text back instead of being done.
    Ran::Said(match verb {
        "help" | "" => help(root),
        "undo" => return undo_command(checkpoints, rest).await,
        "init" => {
            let path = crate::config::project_instructions_path(root);
            if path.exists() {
                format!(
                    "{} is already there; edit it rather than overwrite it",
                    path.display()
                )
            } else {
                match std::fs::write(&path, crate::config::draft_instructions(root)) {
                    Err(e) => format!("could not write {}: {e}", path.display()),
                    Ok(()) => format!(
                        "wrote {}: edit it, then start a session to have it read",
                        path.display()
                    ),
                }
            }
        }
        "diff" => match checkpoints {
            None => "no change history for this session".into(),
            Some(cp) => match cp.diff().await {
                Err(e) => format!("diff failed: {e:#}"),
                Ok(d) if d.trim().is_empty() => {
                    "the agent has not changed anything this session".into()
                }
                Ok(d) => d,
            },
        },
        other => format!("No such command: `/{other}`.\n\n{}", help(root)),
    })
}

/// The commands, the project's own listed after the built-ins.
pub(crate) fn help(root: &std::path::Path) -> String {
    // One table, so the columns line up down the page, with each group a row of its own.
    let mut out = String::from("## Commands\n\n| Command | What it does |\n|---|---|\n");
    for (group, names) in GROUPS {
        out.push_str(&format!("| **{group}** | |\n"));
        for line in HELP.lines().filter_map(|l| l.strip_prefix('/')) {
            let (name, about) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
            if names.contains(&name) {
                out.push_str(&format!("| `/{name}` | {} |\n", about.trim()));
            }
        }
    }
    out.push('\n');
    let mine = crate::config::user_commands(root);
    if !mine.is_empty() {
        out.push_str("## This project's commands\n\n");
        for name in mine {
            out.push_str(&format!("- `/{name}`\n"));
        }
    }
    out.push_str("\n## Keys\n\n| Key | What it does |\n|---|---|\n");
    for (key, about) in KEYS {
        out.push_str(&format!("| `{key}` | {about} |\n"));
    }
    out.push_str(
        "\n## On the line\n\n\
         - `@path` sends that file with the question\n\
         - `!command` runs a command here, and the model sees what it printed\n\
         - `#rule` writes a rule for this project and tells the model at once\n",
    );
    out
}

/// The commands by what they are for, in the order `/help` lists them. Every command of
/// the help text is in one group.
const GROUPS: [(&str, &[&str]); 4] = [
    ("Asking", &["plan", "act", "retry", "model", "compact"]),
    ("Changes", &["diff", "undo", "approve"]),
    ("The session", &["clear", "resume", "copy", "init"]),
    (
        "Where things stand",
        &[
            "why", "cost", "context", "jobs", "digest", "tools", "mcp", "help",
        ],
    ),
];

/// The commands that take an argument, which completion leaves a space after.
pub(crate) const TAKES_ARGUMENT: [&str; 7] =
    ["model", "retry", "mcp", "digest", "copy", "undo", "resume"];

/// The keys, as `/help` lists them.
const KEYS: [(&str, &str); 15] = [
    ("shift-tab", "act, write without asking, plan"),
    ("enter", "send, or take what the list offers"),
    ("shift-enter", "a new line"),
    (
        "esc",
        "stop the turn, close a list, clear the line; twice, the last question back",
    ),
    ("alt-up alt-down", "move between the steps of the session"),
    ("space", "unfold what was counted, while pointing at it"),
    ("u d", "on a receipt, put the turn back or see its diff"),
    ("ctrl-b", "show or hide the rail"),
    ("ctrl-f", "find something said earlier"),
    ("ctrl-g", "write the prompt in your editor"),
    ("ctrl-y", "take what is selected, or the block pointed at"),
    ("ctrl-t", "lend the mouse to the terminal, or take it back"),
    (
        "y a n e x",
        "answer a change: once, always, no, no and why, no and fix it",
    ),
    (
        "alt-enter",
        "while a turn runs, ask it once the turn is over",
    ),
    ("ctrl-c", "stop, clear the line, or leave"),
];

/// The commands, as a list to choose from.
///
/// Read off the same text the help prints, so a command cannot be offered by one and
/// missing from the other.
pub(crate) fn command_choices(root: &std::path::Path) -> Vec<crate::ui::picker::Choice> {
    let mut out: Vec<crate::ui::picker::Choice> = HELP
        .lines()
        .filter_map(|l| l.strip_prefix('/'))
        .map(|l| match l.split_once(char::is_whitespace) {
            Some((name, about)) => crate::ui::picker::Choice::new(name.trim(), about.trim()),
            None => crate::ui::picker::Choice::new(l.trim(), ""),
        })
        .collect();
    for name in crate::config::user_commands(root) {
        out.push(crate::ui::picker::Choice::new(name, "this project's own"));
    }
    out.sort_by(|a, b| a.value.cmp(&b.value));
    out
}

pub(crate) const HELP: &str = "\
/why    why this model is the one answering
/cost   what this session has spent, in tokens and time
/context how full the window is, and what is holding it
/jobs   what is running now, to go to its step
/compact elide the oldest steps now rather than when it fills
/approve what is in force, here and in the configuration, or /approve forget
/plan   read and search, change nothing
/act    leave planning, change things again
/model  what the backend serves, or /model <name> to switch and remember it
/retry  ask the last question again, or /retry <model> to ask another model
/copy   put the conversation on the clipboard and in a file, or /copy last for the answer
/diff   everything the agent has changed this session
/undo   put back the most recent turn that changed something
/digest what has been worked out about this project, or /digest forget
/init   write a first draft of this project's instructions
/tools  the tools this session can call, and where they came from
/mcp    the tool servers, or /mcp <server> <prompt> to take one of their prompts
/clear  start the conversation again, keeping the project and the model
/resume come back to a session recorded earlier, choosing from what there is
/help   this, and any command the project defines";

#[cfg(test)]
mod help_tests {
    use super::*;

    /// Every command the help text names is listed under one group.
    #[test]
    fn every_command_has_a_group() {
        for line in HELP.lines().filter_map(|l| l.strip_prefix('/')) {
            let name = line.split_whitespace().next().unwrap_or("");
            let groups = GROUPS.iter().filter(|(_, n)| n.contains(&name)).count();
            assert_eq!(groups, 1, "/{name} is in {groups} groups");
        }
    }
}

#[cfg(test)]
mod shell_tests {
    use super::*;
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use tokio_util::sync::CancellationToken;

    /// Each line reaches the screen as it is printed, from either stream, and the body and
    /// the failure are what the command said and how it ended.
    #[tokio::test]
    async fn a_command_of_your_own_prints_as_it_goes() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let kept = seen.clone();
        let (is_error, body) = run_shell(
            "echo one; echo two >&2; exit 3",
            Path::new("."),
            4096,
            CancellationToken::new(),
            move |line| kept.lock().unwrap().push(line),
        )
        .await;
        let mut seen = seen.lock().unwrap().clone();
        seen.sort();
        assert_eq!(seen, ["one", "two"]);
        assert!(is_error);
        assert!(body.contains("one") && body.contains("two"), "{body}");
    }

    /// A command stopped ends at once and says so.
    #[tokio::test]
    async fn a_command_of_your_own_stops_when_asked() {
        let stop = CancellationToken::new();
        stop.cancel();
        let (is_error, body) = run_shell("sleep 30", Path::new("."), 4096, stop, |_| {}).await;
        assert!(is_error);
        assert_eq!(body, "stopped before it finished");
    }
}
