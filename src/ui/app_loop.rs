use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Terminal;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tui_textarea::TextArea;

use crate::agent::agent_loop::AgentEvent;
use crate::agent::protocol::ToolProtocol;
use crate::app::AppConfig;
use crate::boot;
use crate::checkpoint::Checkpoints;
use crate::sandbox::approval::{ApprovalDecision, ApprovalPrompt};
use crate::ui::approval::ApprovalView;
use crate::ui::chat::Entry;
use crate::ui::status::Status;
use crate::ui::term;

/// How long to let the agent finish what it is doing after being asked to stop.
///
/// It is asked, not killed: aborting the task can drop it inside a filesystem write and
/// leave a file half-written. The grace period bounds how long a wedged tool can delay
/// the exit before the process leaves anyway.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// One user turn handed to the agent, with the handle that abandons it.
///
/// The token is minted by the UI rather than the agent, because the side that has to
/// react to `Esc` is the side that must already be holding it.
struct TurnRequest {
    text: String,
    cancel: CancellationToken,
}

pub async fn run(cfg: AppConfig) -> Result<()> {
    let boot::Booted {
        mut agent,
        lsp,
        mut approval_rx,
        shutdown,
        recorder,
        session_id,
        model,
        protocol,
        restored,
        checkpoints,
        checkpoint_error,
        mcp,
    } = boot::boot(&cfg).await?;

    // Channels:
    //   input_tx → agent task ── pushes "user said X"
    //   agent_evt_tx → UI     ── streaming text + tool events
    let (input_tx, mut input_rx) = mpsc::channel::<TurnRequest>(4);
    let (agent_evt_tx, mut agent_evt_rx) = mpsc::channel::<AgentEvent>(128);
    let agent_evt_tx_for_loop = agent_evt_tx.clone();
    let agent_task = tokio::spawn(async move {
        while let Some(TurnRequest { text, cancel }) = input_rx.recv().await {
            if let Err(e) = agent
                .run_user_turn(text, cancel, agent_evt_tx_for_loop.clone())
                .await
            {
                let _ = agent_evt_tx_for_loop
                    .send(AgentEvent::Notice(format!("agent error: {e:#}")))
                    .await;
                let _ = agent_evt_tx_for_loop.send(AgentEvent::TurnDone).await;
            }
        }
    });

    // The hook goes in before the terminal is taken, so the window in which a panic could
    // escape uncovered never opens.
    term::install_panic_hook();
    let guard = term::TerminalGuard::new()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;

    let result = run_ui(
        &mut terminal,
        &cfg,
        &model,
        protocol,
        checkpoints,
        checkpoint_error,
        mcp.notices.clone(),
        session_id,
        restored,
        &shutdown,
        input_tx,
        &mut agent_evt_rx,
        &mut approval_rx,
    )
    .await;

    // Hand the screen back before anything else can print to it.
    drop(guard);

    // Ask the agent to stop rather than aborting it: the task could be inside a
    // filesystem write, and dropping it there leaves a truncated file behind. Dropping
    // `input_tx` above closed the channel, so the agent's own loop ends once the turn in
    // flight notices the cancellation.
    shutdown.cancel();
    // Language servers are killed on drop either way; this gives them the chance to put
    // their own caches down cleanly first.
    let _ = tokio::time::timeout(SHUTDOWN_GRACE, lsp.shutdown()).await;
    for client in &mcp.clients {
        client.shutdown().await;
    }
    let agent_stopped = tokio::time::timeout(SHUTDOWN_GRACE, agent_task)
        .await
        .is_ok();

    // The agent owned the session, so its ending closed the transcript. Waiting for the
    // writer here is what gets the last few records onto disk: without it the process
    // exits first and a session loses precisely the part someone wanted to read.
    if let Some(recorder) = recorder {
        if agent_stopped {
            let _ = tokio::time::timeout(SHUTDOWN_GRACE, recorder).await;
        } else {
            recorder.abort();
        }
    }

    if !agent_stopped {
        eprintln!(
            "verve: the agent did not stop within {}s; exiting anyway",
            SHUTDOWN_GRACE.as_secs()
        );
    }
    result
}

#[derive(Default)]
struct UiState {
    entries: Vec<Entry>,
    scroll: u16,
    pending_approval: Option<ApprovalPrompt>,
    busy: bool,
    step: usize,
    /// Abandons the turn in flight. `None` between turns.
    turn_cancel: Option<CancellationToken>,
}

#[allow(clippy::too_many_arguments)]
async fn run_ui(
    term: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    cfg: &AppConfig,
    model: &str,
    protocol: ToolProtocol,
    checkpoints: Option<Arc<Checkpoints>>,
    checkpoint_error: Option<String>,
    mcp_notices: Vec<String>,
    session_id: String,
    restored: usize,
    shutdown: &CancellationToken,
    input_tx: mpsc::Sender<TurnRequest>,
    agent_evt_rx: &mut mpsc::Receiver<AgentEvent>,
    approval_rx: &mut mpsc::Receiver<ApprovalPrompt>,
) -> Result<()> {
    let mut state = UiState::default();
    state.entries.push(Entry::Notice(format!(
        "Welcome. Enter sends, Shift-Enter is a newline, Esc interrupts a running turn, \
         PageUp/PageDown scroll, Ctrl-C or Ctrl-Q quits.\n\
         Backend: {} · tool calls: {}",
        cfg.backend_url,
        protocol.label()
    )));
    if restored > 0 {
        state.entries.push(Entry::Notice(format!(
            "resumed session {session_id}: {restored} earlier message(s) are back in view"
        )));
    }
    if let Some(why) = &checkpoint_error {
        state.entries.push(Entry::Notice(format!(
            "no change history for this session ({why}); /undo and /diff are unavailable"
        )));
    }
    for notice in &mcp_notices {
        state.entries.push(Entry::Notice(notice.clone()));
    }
    // Raised while resolving settings, before there was a screen to say them on.
    for warning in &cfg.warnings {
        state
            .entries
            .push(Entry::Notice(format!("warning: {warning}")));
    }

    let mut input = TextArea::default();
    input.set_placeholder_text(
        "Ask the model to do something… (Enter to send, Shift-Enter for newline)",
    );
    input.set_block(Block::default().borders(Borders::ALL).title(" input "));

    let mut events = EventStream::new();

    loop {
        term.draw(|f| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(3),
                    Constraint::Length(5),
                    Constraint::Length(1),
                ])
                .split(f.area());

            let mut lines: Vec<Line<'static>> = Vec::new();
            for e in &state.entries {
                lines.extend(e.to_lines());
            }
            let chat_block = Block::default().borders(Borders::ALL).title(" chat ");
            let inner_h = chat_block.inner(chunks[0]).height as usize;
            let total = lines.len();
            let scroll = state.scroll as usize;
            let end = total.saturating_sub(scroll);
            let start = end.saturating_sub(inner_h);
            let visible: Vec<Line<'static>> =
                lines.into_iter().skip(start).take(end - start).collect();
            let chat = Paragraph::new(visible)
                .block(chat_block)
                .wrap(Wrap { trim: false });
            f.render_widget(chat, chunks[0]);

            f.render_widget(&input, chunks[1]);

            let status = Status {
                model: model.to_string(),
                root: cfg.root.display().to_string(),
                session: session_id.clone(),
                grammar: cfg.grammar_enabled,
                step: state.step,
                max_steps: cfg.max_steps,
                busy: state.busy,
            };
            f.render_widget(
                Paragraph::new(status.to_line()).style(Style::default().fg(Color::Gray)),
                chunks[2],
            );

            if let Some(prompt) = state.pending_approval.as_ref() {
                ApprovalView { req: &prompt.req }.render(f, f.area());
            }
        })?;

        tokio::select! {
            biased;

            Some(prompt) = approval_rx.recv() => {
                state.pending_approval = Some(prompt);
            }

            Some(evt) = agent_evt_rx.recv() => {
                match evt {
                    AgentEvent::AssistantDelta(chunk) => {
                        match state.entries.last_mut() {
                            Some(Entry::AssistantStreaming(s)) => s.push_str(&chunk),
                            _ => state.entries.push(Entry::AssistantStreaming(chunk)),
                        }
                    }
                    AgentEvent::AssistantText(s) => {
                        if let Some(Entry::AssistantStreaming(_)) = state.entries.last() {
                            state.entries.pop();
                        }
                        if !s.trim().is_empty() {
                            state.entries.push(Entry::Assistant(s));
                        }
                    }
                    AgentEvent::ToolCall { name, args_pretty } => {
                        state.step += 1;
                        state.entries.push(Entry::ToolCall { name, args: args_pretty });
                    }
                    AgentEvent::ToolResult { name, output } => {
                        state.entries.push(Entry::ToolResult {
                            name,
                            is_error: output.is_error,
                            body: output.content,
                        });
                    }
                    AgentEvent::Notice(s) => state.entries.push(Entry::Notice(s)),
                    AgentEvent::TurnDone => {
                        state.busy = false;
                        state.step = 0;
                        state.turn_cancel = None;
                        // A turn only ends while a prompt is up if it was abandoned: the
                        // tool that asked has already given up waiting, so the modal is
                        // asking about something that will not happen either way.
                        state.pending_approval = None;
                    }
                }
            }

            maybe_evt = events.next() => {
                let Some(Ok(evt)) = maybe_evt else { continue };
                if let Event::Key(key) = evt {
                    // Quit is tested before anything else can swallow the key. It used to
                    // sit after the approval branch, which returned early - so the one
                    // screen the user most wants to escape from was the one screen where
                    // Ctrl-C did nothing.
                    if is_quit(&key) {
                        return Ok(());
                    }

                    if state.pending_approval.is_some() {
                        if let Some(decision) = approval_key(&key) {
                            if let Some(prompt) = state.pending_approval.take() {
                                let _ = prompt.reply.send(decision);
                            }
                        }
                        continue;
                    }

                    if key.kind == KeyEventKind::Press {
                        match key.code {
                            KeyCode::Esc if state.busy => {
                                if let Some(cancel) = state.turn_cancel.take() {
                                    cancel.cancel();
                                    state.entries.push(Entry::Notice("interrupting…".into()));
                                }
                                continue;
                            }
                            KeyCode::PageUp   => { state.scroll = state.scroll.saturating_add(5); continue; }
                            KeyCode::PageDown => { state.scroll = state.scroll.saturating_sub(5); continue; }
                            KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                                if state.busy { continue; }
                                let text: String = input.lines().join("\n");
                                if text.trim().is_empty() { continue; }
                                state.entries.push(Entry::User(text.clone()));
                                input = TextArea::default();
                                input.set_block(Block::default().borders(Borders::ALL).title(" input "));
                                // A line beginning with `/` is for verve, not the model.
                                // Handled before the turn is started so that `/undo` works
                                // when the thing you want undone is what the model just did.
                                if let Some(command) = text.strip_prefix('/') {
                                    let reply = run_command(command.trim(), checkpoints.as_deref()).await;
                                    state.entries.push(Entry::Notice(reply));
                                    continue;
                                }
                                state.busy = true;
                                state.step = 0;
                                // A child of the shutdown token, so quitting cancels the
                                // turn in flight as well as this one's own Esc.
                                let cancel = shutdown.child_token();
                                state.turn_cancel = Some(cancel.clone());
                                let _ = input_tx.send(TurnRequest { text, cancel }).await;
                                continue;
                            }
                            _ => {}
                        }
                    }
                    input.input(key);
                }
            }

            _ = tokio::time::sleep(Duration::from_millis(80)) => {}
        }
    }
}

fn is_quit(k: &KeyEvent) -> bool {
    if k.kind != KeyEventKind::Press {
        return false;
    }
    matches!(
        (k.code, k.modifiers),
        (KeyCode::Char('c'), KeyModifiers::CONTROL) | (KeyCode::Char('q'), KeyModifiers::CONTROL)
    )
}

/// Answer a slash command.
///
/// These do not reach the model and do not become part of the conversation: they are things
/// the user is asking of the tool about the session, and putting them in the transcript
/// would describe a turn that never happened.
async fn run_command(command: &str, checkpoints: Option<&Checkpoints>) -> String {
    match command {
        "help" | "" => HELP.into(),
        "undo" => match checkpoints {
            None => "no change history for this session".into(),
            Some(cp) => match cp.undo().await {
                Err(e) => format!("undo failed: {e:#}"),
                Ok(None) => "nothing to undo: the agent has not changed anything".into(),
                Ok(Some(done)) => {
                    if done.files.is_empty() {
                        format!("turn {} put back", done.turn)
                    } else {
                        format!(
                            "turn {} put back, {} file(s):\n{}",
                            done.turn,
                            done.files.len(),
                            done.files.join("\n")
                        )
                    }
                }
            },
        },
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
        other => format!("no such command: /{other}\n{HELP}"),
    }
}

const HELP: &str = "\
/diff   everything the agent has changed this session
/undo   put back the most recent turn that changed something
/help   this";

/// Which decision a keystroke stands for at the approval modal.
///
/// `Enter` is deliberately absent. It is also the send key, so a user mid-sentence when
/// the modal appears would approve a shell command they never read - the one outcome an
/// approval flow exists to prevent. Approving is a letter you have to reach for.
fn approval_key(k: &KeyEvent) -> Option<ApprovalDecision> {
    if k.kind != KeyEventKind::Press {
        return None;
    }
    match k.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => Some(ApprovalDecision::Once),
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(ApprovalDecision::Deny),
        KeyCode::Char('a') | KeyCode::Char('A') => Some(ApprovalDecision::Always),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    /// Enter is the send key. If it also approves, a user who is mid-sentence when a
    /// prompt appears authorises a shell command they never read.
    #[test]
    fn enter_does_not_approve() {
        assert_eq!(approval_key(&press(KeyCode::Enter)), None);
    }

    /// Declining must stay reachable by the two gestures people already use to back out.
    #[test]
    fn declining_is_reachable() {
        assert_eq!(
            approval_key(&press(KeyCode::Esc)),
            Some(ApprovalDecision::Deny)
        );
        assert_eq!(
            approval_key(&press(KeyCode::Char('n'))),
            Some(ApprovalDecision::Deny)
        );
    }

    #[test]
    fn approving_takes_a_letter() {
        assert_eq!(
            approval_key(&press(KeyCode::Char('y'))),
            Some(ApprovalDecision::Once)
        );
        assert_eq!(
            approval_key(&press(KeyCode::Char('Y'))),
            Some(ApprovalDecision::Once)
        );
        assert_eq!(
            approval_key(&press(KeyCode::Char('a'))),
            Some(ApprovalDecision::Always)
        );
    }

    /// A key with no meaning at the modal must leave it up rather than pick for the user.
    #[test]
    fn an_unrelated_key_decides_nothing() {
        assert_eq!(approval_key(&press(KeyCode::Char('k'))), None);
        assert_eq!(approval_key(&press(KeyCode::Tab)), None);
    }

    /// Quit is consulted before the approval branch, so these must be recognised as quit
    /// even though the modal would otherwise read them as "not a decision".
    #[test]
    fn quit_is_recognised() {
        assert!(is_quit(&ctrl('c')));
        assert!(is_quit(&ctrl('q')));
        assert!(!is_quit(&press(KeyCode::Char('c'))));
        assert_eq!(approval_key(&ctrl('c')), None);
    }
}
