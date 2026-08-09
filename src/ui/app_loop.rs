use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Terminal;
use tokio::sync::mpsc;
use tui_textarea::TextArea;

use crate::agent::agent_loop::{Agent, AgentConfig, AgentEvent};
use crate::agent::session::Session;
use crate::app::AppConfig;
use crate::backend::client::BackendClient;
use crate::sandbox::approval::{self, ApprovalDecision, ApprovalPrompt, SessionApprovals};
use crate::tools::lsp::LspPool;
use crate::tools::registry::{Registry, ToolCtx};
use crate::ui::approval::ApprovalView;
use crate::ui::chat::Entry;
use crate::ui::status::Status;

pub async fn run(cfg: AppConfig) -> Result<()> {
    let registry = Arc::new(build_registry(cfg.root.clone()));
    let (approval_tx, mut approval_rx) = approval::channel();
    let session_approvals = Arc::new(SessionApprovals::new());

    let ctx = ToolCtx {
        root: cfg.root.clone(),
        approval: approval_tx.clone(),
        session_approvals: session_approvals.clone(),
    };

    let system = Agent::build_system_prompt(&registry);
    let session = Session::new(system, cfg.context_tokens, cfg.max_answer_tokens);
    let session_id = session.id.clone();

    let agent_cfg = AgentConfig {
        model: cfg.model.clone(),
        grammar_enabled: cfg.grammar_enabled,
        max_steps: cfg.max_steps,
        max_answer_tokens: cfg.max_answer_tokens,
    };

    let mut agent = Agent {
        backend: BackendClient::new(cfg.backend_url.clone()),
        registry: registry.clone(),
        session,
        cfg: agent_cfg,
        ctx,
    };

    // Channels:
    //   input_tx → agent task ── pushes "user said X"
    //   agent_evt_tx → UI     ── streaming text + tool events
    let (input_tx, mut input_rx) = mpsc::channel::<String>(4);
    let (agent_evt_tx, mut agent_evt_rx) = mpsc::channel::<AgentEvent>(128);
    let agent_evt_tx_for_loop = agent_evt_tx.clone();
    let agent_task = tokio::spawn(async move {
        while let Some(text) = input_rx.recv().await {
            if let Err(e) = agent.run_user_turn(text, agent_evt_tx_for_loop.clone()).await {
                let _ = agent_evt_tx_for_loop
                    .send(AgentEvent::Notice(format!("agent error: {e:#}")))
                    .await;
                let _ = agent_evt_tx_for_loop.send(AgentEvent::TurnDone).await;
            }
        }
    });

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;

    let result = run_ui(
        &mut term,
        &cfg,
        session_id,
        input_tx,
        &mut agent_evt_rx,
        &mut approval_rx,
    )
    .await;

    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    term.show_cursor()?;
    agent_task.abort();
    result
}

fn build_registry(root: std::path::PathBuf) -> Registry {
    use crate::tools::{
        bash::Bash,
        fs_read::{Glob, ListDir, ReadFile},
        fs_write::{ApplyPatch, EditFile, WriteFile},
        lsp::{LspDiagnostics, LspHover},
        search::Grep,
        tests::RunTests,
        web_fetch::WebFetch,
    };

    let mut reg = Registry::new();
    reg.register(ReadFile);
    reg.register(ListDir);
    reg.register(Glob);
    reg.register(Grep);
    reg.register(WriteFile);
    reg.register(EditFile);
    reg.register(ApplyPatch);
    reg.register(Bash);
    reg.register(RunTests);
    reg.register(WebFetch);
    let pool = LspPool::new(root);
    reg.register(LspDiagnostics { pool: pool.clone() });
    reg.register(LspHover { pool });
    reg
}

#[derive(Default)]
struct UiState {
    entries: Vec<Entry>,
    scroll: u16,
    pending_approval: Option<ApprovalPrompt>,
    busy: bool,
    step: usize,
}

async fn run_ui(
    term: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    cfg: &AppConfig,
    session_id: String,
    input_tx: mpsc::Sender<String>,
    agent_evt_rx: &mut mpsc::Receiver<AgentEvent>,
    approval_rx: &mut mpsc::Receiver<ApprovalPrompt>,
) -> Result<()> {
    let mut state = UiState::default();
    state.entries.push(Entry::Notice(format!(
        "Welcome. Type your request. Ctrl-C or Ctrl-Q to quit. Backend: {}",
        cfg.backend_url
    )));

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
            let visible: Vec<Line<'static>> = lines.into_iter().skip(start).take(end - start).collect();
            let chat = Paragraph::new(visible)
                .block(chat_block)
                .wrap(Wrap { trim: false });
            f.render_widget(chat, chunks[0]);

            f.render_widget(&input, chunks[1]);

            let status = Status {
                model: cfg.model.clone(),
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
                    }
                }
            }

            maybe_evt = events.next() => {
                let Some(Ok(evt)) = maybe_evt else { continue };
                if let Event::Key(key) = evt {
                    if state.pending_approval.is_some() {
                        if let Some(decision) = approval_key(&key) {
                            if let Some(prompt) = state.pending_approval.take() {
                                let _ = prompt.reply.send(decision);
                            }
                        }
                        continue;
                    }

                    if is_quit(&key) {
                        return Ok(());
                    }
                    if key.kind == KeyEventKind::Press {
                        match key.code {
                            KeyCode::PageUp   => { state.scroll = state.scroll.saturating_add(5); continue; }
                            KeyCode::PageDown => { state.scroll = state.scroll.saturating_sub(5); continue; }
                            KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                                if state.busy { continue; }
                                let text: String = input.lines().join("\n");
                                if text.trim().is_empty() { continue; }
                                state.entries.push(Entry::User(text.clone()));
                                input = TextArea::default();
                                input.set_block(Block::default().borders(Borders::ALL).title(" input "));
                                state.busy = true;
                                state.step = 0;
                                let _ = input_tx.send(text).await;
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
    if k.kind != KeyEventKind::Press { return false; }
    matches!(
        (k.code, k.modifiers),
        (KeyCode::Char('c'), KeyModifiers::CONTROL)
            | (KeyCode::Char('q'), KeyModifiers::CONTROL)
    )
}

fn approval_key(k: &KeyEvent) -> Option<ApprovalDecision> {
    if k.kind != KeyEventKind::Press { return None; }
    match k.code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => Some(ApprovalDecision::Once),
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc   => Some(ApprovalDecision::Deny),
        KeyCode::Char('a') | KeyCode::Char('A')                  => Some(ApprovalDecision::Always),
        _ => None,
    }
}
