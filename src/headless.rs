//! One turn, no interface.
//!
//! What makes an agent usable from a git hook, a pipeline or a shell script rather than only
//! from a terminal somebody is sitting at. The hard part is not the absence of a screen: it
//! is that there is nobody to answer an approval, so the policy has to be stated up front and
//! stated narrowly, and the exit code has to mean something.

use std::io::Write;

use anyhow::Result;
use serde::Serialize;

use crate::agent::agent_loop::AgentEvent;
use crate::app::AppConfig;
use crate::boot;
use crate::sandbox::approval::ApprovalDecision;

/// What a run with nobody watching may agree to.
///
/// Read-only tools never prompt, so this governs only the ones that change something, run
/// something or leave the machine. The default refuses all of them: an agent let loose by a
/// script with nobody to stop it should have to be told, in writing, what it may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ApprovePolicy {
    /// Decline everything that would change anything. The agent can still read and search.
    None,
    /// Allow file writes, edits and patches. Not the shell, the test suite or the network.
    Edits,
    /// Allow everything, including running commands.
    All,
}

impl ApprovePolicy {
    fn decide(self, tool: &str) -> ApprovalDecision {
        let allowed = match self {
            ApprovePolicy::None => false,
            ApprovePolicy::Edits => {
                matches!(tool, "write_file" | "edit_file" | "apply_patch")
            }
            ApprovePolicy::All => true,
        };
        if allowed {
            // `Once`, not `Always`: every call is judged against the policy on its own, so
            // one approved write cannot widen into a standing agreement nobody is present
            // to have made.
            ApprovalDecision::Once
        } else {
            ApprovalDecision::Deny(None)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
}

/// What the run did, for a caller that has to decide something from it.
#[derive(Debug, Serialize)]
struct Report {
    session: String,
    model: String,
    protocol: String,
    /// The model's last prose, which is the answer to the prompt.
    result: String,
    tool_calls: Vec<ToolRecord>,
    notices: Vec<String>,
    /// True when a tool was refused by the policy, so a caller can tell "it decided not to"
    /// from "it was not allowed to".
    blocked: bool,
    ok: bool,
}

#[derive(Debug, Serialize)]
struct ToolRecord {
    name: String,
    error: bool,
    output: String,
}

/// Exit codes, so a script can branch without parsing anything.
pub mod exit {
    /// The turn finished and the model stopped asking for tools.
    pub const OK: i32 = 0;
    /// The turn ran but something in it failed - a backend error, or the step limit.
    pub const FAILED: i32 = 1;
    /// A tool was refused by the approval policy.
    pub const BLOCKED: i32 = 2;
}

pub async fn run(
    cfg: AppConfig,
    prompt: String,
    policy: ApprovePolicy,
    format: OutputFormat,
) -> Result<i32> {
    let boot::Booted {
        mut agent,
        lsp,
        mut approval_rx,
        shutdown,
        recorder,
        session_id,
        model,
        protocol,
        checkpoint_error,
        mut mcp,
        instruction_notes,
        ..
    } = boot::boot(&cfg).await?;

    if let Some(why) = &checkpoint_error {
        tracing::warn!(%why, "running without a change history");
    }

    let mut blocked = false;
    // Answering from the policy rather than from a person. Every prompt is still raised and
    // still recorded, so the log says what was asked and what the policy said to it.
    let (blocked_tx, mut blocked_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let approver = tokio::spawn(async move {
        while let Some(prompt) = approval_rx.recv().await {
            let decision = policy.decide(&prompt.req.tool);
            tracing::info!(
                tool = %prompt.req.tool,
                ?decision,
                summary = %prompt.req.summary,
                "headless approval"
            );
            if matches!(decision, ApprovalDecision::Deny(_)) {
                let _ = blocked_tx.send(prompt.req.tool.clone());
            }
            let _ = prompt.reply.send(decision);
        }
    });

    // Asked before the turn rather than discovered during it. A run with one prompt has
    // no second chance: a step spent finding out that the backend drops the tool
    // definitions is a step of the budget, and its answer is a fragment of a tool call
    // presented as prose.
    if cfg.tool_protocol == crate::app::ProtocolChoice::Auto
        && crate::backend::probe::ignores_native_tools(
            agent.backend.clone(),
            &agent.cfg.model.clone(),
            shutdown.child_token(),
        )
        .await
    {
        agent
            .control(crate::agent::agent_loop::Control::Protocol(
                crate::agent::protocol::ToolProtocol::Envelope,
            ))
            .await;
        mcp.notices.push(
            "the backend accepts tool definitions and ignores them; tool calls travel as \
             the JSON envelope"
                .to_string(),
        );
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(128);
    let cancel = shutdown.child_token();
    let driver = tokio::spawn(async move { agent.run_user_turn(prompt, cancel, tx).await });

    let mut report = Report {
        session: session_id,
        model,
        protocol: protocol.label().into(),
        result: String::new(),
        tool_calls: Vec::new(),
        notices: Vec::new(),
        blocked: false,
        ok: true,
    };
    let text_output = format == OutputFormat::Text;

    // What settling the session decided: which window the backend offers, where the
    // standing instructions came from, what was capped. The interface shows these and a
    // headless run dropped them, so a caller had no way to learn that its window had been
    // settled at something other than what it asked for.
    for notice in &instruction_notes {
        tracing::info!(%notice, "settled");
        if text_output {
            eprintln!("· {notice}");
        }
        report.notices.push(notice.clone());
    }

    // Said before the turn rather than after it: a tool server that would not start, or a
    // tool that was refused, explains what the model then failed to do. It belongs in the
    // report too, or a caller reading only JSON cannot see it at all.
    for notice in &mcp.notices {
        tracing::warn!(%notice, "mcp");
        if text_output {
            eprintln!("· {notice}");
        }
        report.notices.push(notice.clone());
    }

    while let Some(event) = rx.recv().await {
        match event {
            // Where the turn is, for somebody watching a pipeline. Narration, so it goes
            // to stderr with the rest and never into the report.
            AgentEvent::Phase(phase) => {
                if text_output {
                    eprintln!("· {}", phase.label());
                }
            }
            // Deltas are the raw stream, which under the envelope protocol is a JSON object
            // being typed out. The prose is reported once it is known.
            AgentEvent::AssistantDelta(_) => {}
            // A scratchpad is not a result. It goes to stderr with the rest of the
            // narration under `text`, and never into the report a caller parses.
            AgentEvent::Reasoning(s) => {
                if text_output {
                    eprint!("{s}");
                }
            }
            AgentEvent::AssistantText(s) => {
                if text_output && !s.trim().is_empty() {
                    println!("{s}");
                }
                report.result = s;
            }
            AgentEvent::ToolCall { name, args_pretty } => {
                if text_output {
                    eprintln!("→ {name} {}", one_line(&args_pretty));
                }
            }
            AgentEvent::ToolResult { name, output } => {
                if text_output {
                    eprintln!("← {name}{}", if output.is_error { " [error]" } else { "" });
                }
                report.tool_calls.push(ToolRecord {
                    name,
                    error: output.is_error,
                    output: output.content,
                });
            }
            // A headless run has nobody typing, so this can only arrive from a caller
            // that steered on purpose; narrated like the rest.
            AgentEvent::Steered(s) => {
                if text_output {
                    eprintln!("» {}", one_line(&s));
                }
            }
            // What the backend turned out to do, which changes how the turn was run and
            // belongs with the rest of what the run has to say for itself.
            // A number for a status line nobody is watching.
            AgentEvent::Window(_) => {}
            AgentEvent::Protocol(protocol) => {
                report
                    .notices
                    .push(format!("tool calls travel as the {}", protocol.label()));
            }
            AgentEvent::Notice(s) => {
                if text_output {
                    eprintln!("· {s}");
                }
                // A step limit or a backend error reaches the caller as a notice; neither is
                // a run that did what was asked.
                if s.contains("max_steps") || s.contains("backend error") {
                    report.ok = false;
                }
                report.notices.push(s);
            }
            AgentEvent::TurnDone => break,
        }
    }

    while let Ok(tool) = blocked_rx.try_recv() {
        blocked = true;
        report
            .notices
            .push(format!("refused by --approve policy: {tool}"));
    }
    report.blocked = blocked;

    let turn = driver.await;
    if let Ok(Err(e)) = &turn {
        report.ok = false;
        report.notices.push(format!("agent error: {e:#}"));
    }
    if turn.is_err() {
        report.ok = false;
        report.notices.push("the agent task did not finish".into());
    }

    shutdown.cancel();
    approver.abort();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), lsp.shutdown()).await;
    for client in &mcp.clients {
        client.shutdown().await;
    }
    // The agent is finished, so the transcript is closed; waiting for the writer is what
    // gets the last records onto disk before the process leaves.
    if let Some(recorder) = recorder {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(3), recorder).await;
    }

    let code = if !report.ok {
        exit::FAILED
    } else if report.blocked {
        exit::BLOCKED
    } else {
        exit::OK
    };

    if format == OutputFormat::Json {
        let mut out = std::io::stdout().lock();
        serde_json::to_writer_pretty(&mut out, &report)?;
        writeln!(out)?;
    }
    Ok(code)
}

/// Tool arguments on one line, for a progress line that is not the point.
fn one_line(s: &str) -> String {
    const WIDTH: usize = 120;
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.len() <= WIDTH {
        return flat;
    }
    // Cutting by byte count lands inside a character on anything but ASCII, and a `String`
    // truncated there does not exist.
    let cut = (0..=WIDTH)
        .rev()
        .find(|&i| flat.is_char_boundary(i))
        .unwrap_or(0);
    format!("{}…", &flat[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusing_everything_is_the_default_shape() {
        for tool in [
            "write_file",
            "bash",
            "web_fetch",
            "run_tests",
            "apply_patch",
        ] {
            assert_eq!(
                ApprovePolicy::None.decide(tool),
                ApprovalDecision::Deny(None),
                "{tool} was allowed under --approve none"
            );
        }
    }

    /// The point of a middle setting: a hook that may rewrite files should not thereby be
    /// able to run commands or reach the network.
    #[test]
    fn edits_does_not_include_the_shell_or_the_network() {
        assert_eq!(
            ApprovePolicy::Edits.decide("write_file"),
            ApprovalDecision::Once
        );
        assert_eq!(
            ApprovePolicy::Edits.decide("edit_file"),
            ApprovalDecision::Once
        );
        assert_eq!(
            ApprovePolicy::Edits.decide("apply_patch"),
            ApprovalDecision::Once
        );
        for tool in ["bash", "run_tests", "web_fetch"] {
            assert_eq!(
                ApprovePolicy::Edits.decide(tool),
                ApprovalDecision::Deny(None),
                "{tool} was allowed under --approve edits"
            );
        }
    }

    #[test]
    fn all_means_all() {
        for tool in ["write_file", "bash", "run_tests", "web_fetch"] {
            assert_eq!(ApprovePolicy::All.decide(tool), ApprovalDecision::Once);
        }
    }

    /// Never `Always`: one approved call must not widen into a standing agreement made by
    /// nobody.
    #[test]
    fn a_policy_never_grants_a_standing_agreement() {
        for policy in [
            ApprovePolicy::None,
            ApprovePolicy::Edits,
            ApprovePolicy::All,
        ] {
            for tool in ["write_file", "bash", "run_tests", "web_fetch", "edit_file"] {
                assert_ne!(policy.decide(tool), ApprovalDecision::Always);
            }
        }
    }

    #[test]
    fn long_arguments_are_flattened_and_cut() {
        let out = one_line("{\n  \"path\": \"a.rs\"\n}");
        assert_eq!(out, "{ \"path\": \"a.rs\" }");
        assert!(one_line(&"x".repeat(400)).len() <= 124);
        // A multi-byte character must not be cut through.
        let wide = one_line(&"é".repeat(400));
        assert!(wide.ends_with('…'), "unexpected: {wide}");
    }
}
