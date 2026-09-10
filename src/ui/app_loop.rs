use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
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
        instruction_notes,
        mcp,
        steer,
        tools,
        tasks,
        digest,
        discovery,
        mut live,
    } = boot::boot(&cfg).await?;

    // Channels:
    //   input_tx → agent task ── pushes "user said X"
    //   agent_evt_tx → UI     ── streaming text + tool events
    let (input_tx, mut input_rx) = mpsc::channel::<TurnRequest>(4);
    // Asked of the agent between turns, because it owns the session and the interface
    // cannot read it from here.
    let (control_tx, mut control_rx) = mpsc::channel::<crate::agent::agent_loop::Control>(4);
    let (agent_evt_tx, mut agent_evt_rx) = mpsc::channel::<AgentEvent>(128);
    let agent_evt_tx_for_loop = agent_evt_tx.clone();
    // Asked once, while the user is still reading the first screen: does this backend
    // actually do native tool calls? A backend that takes the definitions and drops them
    // fails nothing, so nothing else would ever find out.
    if cfg.tool_protocol == crate::app::ProtocolChoice::Auto {
        let backend = agent.backend.clone();
        let model_probed = agent.cfg.model.clone();
        let tx = control_tx.clone();
        let token = shutdown.child_token();
        let notices = agent_evt_tx.clone();
        tokio::spawn(async move {
            if crate::backend::probe::ignores_native_tools(backend, &model_probed, token).await {
                let _ = tx
                    .send(crate::agent::agent_loop::Control::Protocol(
                        crate::agent::protocol::ToolProtocol::Envelope,
                    ))
                    .await;
                let _ = notices
                    .send(AgentEvent::Notice(
                        "this backend accepts tool definitions and does not use them; \
                         the session speaks the JSON envelope instead"
                            .into(),
                    ))
                    .await;
            }
        });
    }

    let agent_task = tokio::spawn(async move {
        loop {
            // A control request is answered between turns: the turn channel is read
            // first, so nothing can slip a change under a request being built.
            tokio::select! {
                biased;
                turn = input_rx.recv() => {
                    let Some(TurnRequest { text, cancel }) = turn else { break };
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
                request = control_rx.recv() => {
                    let Some(request) = request else { break };
                    agent.control(request).await;
                }
            }
        }
    });

    let startup_notices = [instruction_notes, mcp.notices.clone()].concat();
    let mcp_summary = booted_summary(&mcp);

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
        startup_notices,
        session_id,
        restored,
        &shutdown,
        input_tx,
        control_tx,
        steer,
        tools,
        tasks,
        digest,
        discovery,
        &mut live,
        mcp.clients.clone(),
        mcp_summary,
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
    /// What the turn is doing and since when, for the status line. `None` between turns.
    phase: Option<crate::agent::agent_loop::Phase>,
    /// Read only while a phase is set, so where it starts costs nothing.
    phase_since: Option<Instant>,
    /// Advances on every repaint while busy, which is what moves the spinner.
    tick: usize,
    /// Whether the agent is planning. Held here because the mode lives with the agent,
    /// and the interface only learns of a change by having asked for it.
    planning: bool,
    /// How full the window was when it was last asked, which is at the end of a turn:
    /// the figure only moves when the conversation does.
    context_percent: u8,
    /// The task list as it was last drawn.
    tasks: Vec<crate::tools::plan::Task>,
    /// The model answering now. It changes under the interface - by a command, by the
    /// mode, by a check that failed - so it is asked for rather than assumed.
    model: String,
    /// Abandons the turn in flight. `None` between turns.
    turn_cancel: Option<CancellationToken>,
    /// What the chat pane last drew, so scrolling can be bounded by what exists.
    ///
    /// Known only after a draw, which is why it is remembered rather than computed: the
    /// height comes from the terminal and the wrapping from its width.
    content_lines: usize,
    view_height: usize,
    /// What has been sent this session, oldest first, for the up arrow.
    history: Vec<String>,
    /// Where in that history the arrows are. `None` means at the live input.
    history_at: Option<usize>,
}

impl UiState {
    /// Move towards older lines, stopping where they run out.
    fn scroll_back(&mut self, by: usize) {
        let max = self.content_lines.saturating_sub(self.view_height);
        self.scroll = (self.scroll as usize).saturating_add(by).min(max) as u16;
    }

    fn scroll_forward(&mut self, by: usize) {
        self.scroll = (self.scroll as usize).saturating_sub(by) as u16;
    }

    /// Accumulate a piece of the model's scratchpad into one entry.
    fn push_reasoning(&mut self, chunk: &str) {
        match self.entries.last_mut() {
            Some(Entry::Reasoning(s)) => s.push_str(chunk),
            _ => self.entries.push(Entry::Reasoning(chunk.to_string())),
        }
    }

    fn remember(&mut self, text: &str) {
        // Repeating the last line adds nothing to walk through.
        if self.history.last().map(String::as_str) != Some(text) {
            self.history.push(text.to_string());
        }
        self.history_at = None;
    }

    /// The previous thing sent, or `None` at the beginning of the history.
    fn older(&mut self) -> Option<String> {
        if self.history.is_empty() {
            return None;
        }
        let next = match self.history_at {
            None => self.history.len() - 1,
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.history_at = Some(next);
        self.history.get(next).cloned()
    }

    /// The next thing sent, ending at an empty input rather than on the last entry.
    fn newer(&mut self) -> Option<String> {
        match self.history_at {
            None => None,
            Some(i) if i + 1 < self.history.len() => {
                self.history_at = Some(i + 1);
                self.history.get(i + 1).cloned()
            }
            Some(_) => {
                self.history_at = None;
                Some(String::new())
            }
        }
    }
}

/// Whether the cursor is at the very start of the input.
///
/// The test for "the arrows are free to mean history": anywhere else they are how you move
/// around what you are writing, and taking them would throw away a half-written line.
fn at_start(input: &TextArea) -> bool {
    input.cursor() == (0, 0)
}

fn fresh_input(text: &str) -> TextArea<'static> {
    let mut input = TextArea::from(text.lines().map(String::from).collect::<Vec<_>>());
    input.set_block(Block::default().borders(Borders::ALL).title(" input "));
    input
}

#[allow(clippy::too_many_arguments)]
async fn run_ui(
    term: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    cfg: &AppConfig,
    model: &str,
    protocol: ToolProtocol,
    checkpoints: Option<Arc<Checkpoints>>,
    checkpoint_error: Option<String>,
    startup_notices: Vec<String>,
    session_id: String,
    restored: usize,
    shutdown: &CancellationToken,
    input_tx: mpsc::Sender<TurnRequest>,
    control_tx: mpsc::Sender<crate::agent::agent_loop::Control>,
    // Hands the running turn a line typed while it runs.
    steer: mpsc::Sender<String>,
    // What this session can call, for /tools.
    tools: Vec<String>,

    // The task list the model keeps, drawn while it has anything in it.
    tasks: std::sync::Arc<crate::tools::plan::TaskList>,
    // What is known about the project, shared with the worker that adds to it.
    digest: std::sync::Arc<tokio::sync::Mutex<crate::agent::digest::Digest>>,
    // Answers one question about the project while nobody is waiting.
    discovery: Option<std::sync::Arc<crate::agent::background::Discovery>>,
    // Lines the tools are printing as they print them.
    live: &mut mpsc::Receiver<crate::tools::registry::LiveOutput>,
    // The servers, for the prompts /mcp can take. Cloned handles: the process still owns
    // them and still shuts them down.
    mcp: Vec<std::sync::Arc<crate::mcp::client::McpClient>>,
    mcp_summary: String,
    agent_evt_rx: &mut mpsc::Receiver<AgentEvent>,
    approval_rx: &mut mpsc::Receiver<ApprovalPrompt>,
) -> Result<()> {
    // What the background worker has to say, and the handle that stops it. It runs only
    // while nothing else does, so the token is fired the moment a turn starts: the user's
    // work must never wait behind work nobody asked for.
    let (learn_tx, mut learn_rx) = mpsc::channel::<Option<String>>(4);
    let mut discovering: Option<CancellationToken> = None;

    let mut state = UiState {
        planning: cfg.mode == crate::agent::agent_loop::Mode::Plan,
        model: model.to_string(),
        ..Default::default()
    };
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
    for notice in &startup_notices {
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
            // Remembered for the scroll keys, which cannot know either of these.
            state.content_lines = total;
            state.view_height = inner_h;
            // Scrolling past the top used to blank the pane: the window slid off the end of
            // the content instead of stopping at its oldest line.
            let scroll = (state.scroll as usize).min(total.saturating_sub(inner_h));
            state.scroll = scroll as u16;
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
                model: state.model.clone(),
                root: cfg.root.display().to_string(),
                session: session_id.clone(),
                grammar: cfg.grammar_enabled,
                planning: state.planning,
                context_percent: state.context_percent,
                step: state.step,
                max_steps: cfg.max_steps,
                busy: state.busy,
                phase: state.phase.as_ref().map(|p| p.label()),
                in_phase: state.phase_since.map(|t| t.elapsed()).unwrap_or_default(),
                tick: state.tick,
            };
            f.render_widget(
                Paragraph::new(status.to_line()).style(Style::default().fg(Color::Gray)),
                chunks[2],
            );

            if let Some(prompt) = state.pending_approval.as_ref() {
                ApprovalView { req: &prompt.req }.render(f, f.area());
            }
        })?;

        // Nothing is happening, there is something to find out, and nobody is waiting:
        // the three conditions together are what makes this free. One question at a time,
        // so the machine is never given more than it was idle for.
        if !state.busy && discovering.is_none() {
            if let Some(worker) = &discovery {
                let has_questions = !digest.lock().await.questions.is_empty();
                if has_questions && !worker.stalled() {
                    let token = shutdown.child_token();
                    discovering = Some(token.clone());
                    let worker = worker.clone();
                    let tx = learn_tx.clone();
                    tokio::spawn(async move {
                        let learned = worker.answer_one(token).await;
                        let _ = tx.send(learned).await;
                    });
                }
            }
        }

        // The only timer, and it runs only while a turn does: time passing IS the change
        // to draw then, since the status line counts how long the phase has been going and
        // the spinner has to move. At idle every reason to redraw still arrives as an
        // event - a keystroke, a resize, a piece of the answer, an approval - and this arm
        // is a branch that never completes, so nothing wakes the loop.
        let tick = async {
            if state.busy {
                tokio::time::sleep(Duration::from_millis(120)).await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            biased;

            _ = tick => {
                state.tick = state.tick.wrapping_add(1);
            }

            Some(prompt) = approval_rx.recv() => {
                state.pending_approval = Some(prompt);
            }

            Some(printed) = live.recv() => {
                // Kept as one entry that grows rather than a line each: a build's output
                // is one thing happening, and a hundred entries would push the answer it
                // belongs to off the screen.
                match state.entries.last_mut() {
                    Some(Entry::ToolOutput { name, body }) if *name == printed.tool => {
                        body.push('\n');
                        body.push_str(&printed.line);
                        // Only the tail is kept: the whole of it reaches the model in the
                        // result, and what a person wants while it runs is where it is now.
                        let lines: Vec<&str> = body.lines().collect();
                        if lines.len() > 12 {
                            *body = lines[lines.len() - 12..].join("\n");
                        }
                    }
                    _ => state.entries.push(Entry::ToolOutput {
                        name: printed.tool,
                        body: printed.line,
                    }),
                }
            }

            Some(learned) = learn_rx.recv() => {
                discovering = None;
                // Said once, in a line: what was found is worth knowing, and a paragraph
                // about it in the middle of somebody's work is not.
                if let Some(text) = learned {
                    state.entries.push(Entry::Notice(format!("looked into the project: {text}")));
                }
                for suggestion in digest.lock().await.take_suggestions() {
                    state.entries.push(Entry::Notice(format!("suggestion: {suggestion}")));
                }
            }

            Some(evt) = agent_evt_rx.recv() => {
                match evt {
                    AgentEvent::Phase(phase) => {
                        state.phase = Some(phase);
                        state.phase_since = Some(Instant::now());
                    }
                    AgentEvent::Reasoning(p) => state.push_reasoning(&p),
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
                        // The tail was what it was printing; the result is what it
                        // printed. Keeping both would show the same lines twice.
                        if matches!(
                            state.entries.last(),
                            Some(Entry::ToolOutput { name: running, .. }) if *running == name
                        ) {
                            state.entries.pop();
                        }
                        // The list is the model's, so it is read back after the call that
                        // may have changed it rather than kept in step by hand.
                        if name == "update_plan" {
                            state.tasks = tasks.get().await;
                        }
                        state.entries.push(Entry::ToolResult {
                            name,
                            is_error: output.is_error,
                            body: output.content,
                        });
                    }
                    AgentEvent::Steered(s) => state.entries.push(Entry::Notice(format!(
                        "handed to the model at step {}: {}",
                        state.step + 1,
                        s.lines().next().unwrap_or_default()
                    ))),
                    AgentEvent::Notice(s) => state.entries.push(Entry::Notice(s)),
                    AgentEvent::TurnDone => {
                        // Asked once a turn is over, which is when the conversation has
                        // changed and the agent is free to answer.
                        refresh_status(&control_tx, &mut state).await;
                        state.busy = false;
                        state.step = 0;
                        state.phase = None;
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
                // Mouse capture was turned on from the beginning and nothing ever read an
                // event, so the wheel did nothing in a pane whose purpose is scrolling.
                if let Event::Mouse(mouse) = evt {
                    match mouse.kind {
                        MouseEventKind::ScrollUp => state.scroll_back(3),
                        MouseEventKind::ScrollDown => state.scroll_forward(3),
                        _ => {}
                    }
                    continue;
                }
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
                            KeyCode::PageUp => {
                                state.scroll_back(state.view_height.max(1));
                                continue;
                            }
                            KeyCode::PageDown => {
                                state.scroll_forward(state.view_height.max(1));
                                continue;
                            }
                            KeyCode::Up if at_start(&input) => {
                                if let Some(text) = state.older() {
                                    input = fresh_input(&text);
                                }
                                continue;
                            }
                            KeyCode::Down if at_start(&input) => {
                                if let Some(text) = state.newer() {
                                    input = fresh_input(&text);
                                }
                                continue;
                            }
                            KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                                let text: String = input.lines().join("\n");
                                if text.trim().is_empty() { continue; }
                                state.entries.push(Entry::User(text.clone()));
                                input = TextArea::default();
                                input.set_block(Block::default().borders(Borders::ALL).title(" input "));
                                // A line beginning with `/` is for verve, not the model.
                                // Handled before the turn is started so that `/undo` works
                                // when the thing you want undone is what the model just did.
                                state.remember(&text);
                                let mut text = text;
                                if let Some(command) = text.clone().strip_prefix('/') {
                                    let verb = command.trim_start_matches('/').trim();
                                    let ran = run_command(
                                        command.trim(),
                                        CommandCtx {
                                            checkpoints: checkpoints.as_deref(),
                                            root: &cfg.root,
                                            control: &control_tx,
                                            policy: &cfg.policy,
                                            tools: &tools,
                                            digest: &digest,
                                            mcp: &mcp,
                                            mcp_summary: &mcp_summary,
                                            busy: state.busy,
                                        },
                                    )
                                    .await;
                                    match ran {
                                        Ran::Said(reply) => {
                                            // The mode lives with the agent; the interface
                                            // knows it changed because it was accepted.
                                            if verb.starts_with("plan")
                                                && reply.starts_with("planning")
                                            {
                                                state.planning = true;
                                            }
                                            if verb.starts_with("act")
                                                && reply.starts_with("acting")
                                            {
                                                state.planning = false;
                                            }
                                            state.entries.push(Entry::Notice(reply));
                                            refresh_status(&control_tx, &mut state).await;
                                            continue;
                                        }
                                        // A command the project defines is a way of
                                        // asking, so it becomes this turn's prompt.
                                        Ran::Prompt(prompt) => text = prompt,
                                    }
                                }
                                // Typed while a turn runs, this steers it rather than
                                // starting another: it joins the conversation at the next
                                // step boundary, which is where the history can take it.
                                if state.busy {
                                    if steer.send(text).await.is_err() {
                                        state.entries.push(Entry::Notice(
                                            "the agent is not listening; press Esc to stop the turn"
                                                .into(),
                                        ));
                                    }
                                    continue;
                                }
                                // Whatever was being looked into goes: the user is
                                // waiting now, and the question will still be there.
                                if let Some(token) = discovering.take() {
                                    token.cancel();
                                }
                                state.busy = true;
                                state.step = 0;
                                state.phase = Some(crate::agent::agent_loop::Phase::Preparing);
                                state.phase_since = Some(Instant::now());
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
/// Ask the agent what it is on and how full the window is.
///
/// Both move under the interface, and both are drawn every frame, so they are read back
/// at the moments they can have changed rather than watched.
async fn refresh_status(
    control: &mpsc::Sender<crate::agent::agent_loop::Control>,
    state: &mut UiState,
) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    if control
        .send(crate::agent::agent_loop::Control::Status { reply: tx })
        .await
        .is_err()
    {
        return;
    }
    if let Ok((model, percent)) = rx.await {
        state.model = model;
        state.context_percent = percent;
    }
}

/// What the servers are and what they offer, for `/mcp`.
fn booted_summary(mcp: &crate::mcp::Loaded) -> String {
    if mcp.clients.is_empty() {
        return "no tool servers are configured".to_string();
    }
    let mut lines: Vec<String> = mcp
        .clients
        .iter()
        .map(|c| format!("{}: running", c.server))
        .collect();
    for (server, prompt, about) in &mcp.prompts {
        lines.push(format!(
            "{server}/{prompt}: {about} (/mcp {server} {prompt})"
        ));
    }
    lines.join("\n")
}

/// What running a command came to.
enum Ran {
    /// Something to put on screen.
    Said(String),
    /// Instructions to hand the model as this turn's prompt.
    Prompt(String),
}

/// Everything a command may need. One struct rather than nine arguments, since every new
/// command was adding another to a call nobody could read.
struct CommandCtx<'a> {
    checkpoints: Option<&'a Checkpoints>,
    root: &'a std::path::Path,
    control: &'a mpsc::Sender<crate::agent::agent_loop::Control>,
    policy: &'a crate::sandbox::policy::Policy,
    tools: &'a [String],
    /// What is known about the project, for /digest.
    digest: &'a tokio::sync::Mutex<crate::agent::digest::Digest>,
    mcp: &'a [std::sync::Arc<crate::mcp::client::McpClient>],
    mcp_summary: &'a str,
    /// A control request is answered between turns, so a command that needs one has to
    /// wait rather than sit silent behind a running turn.
    busy: bool,
}

async fn run_command(command: &str, cx: CommandCtx<'_>) -> Ran {
    let CommandCtx {
        checkpoints,
        root,
        control,
        policy,
        tools,
        digest,
        mcp,
        mcp_summary,
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
        let mut d = digest.lock().await;
        if rest == "forget" {
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
            _ => return Ran::Said(mcp_summary.to_string()),
        }
    }
    if verb == "tools" {
        return Ran::Said(tools.join("\n"));
    }
    if verb == "approve" {
        return Ran::Said(policy.describe());
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
    if verb == "model" {
        // The agent reads control requests between turns, so asking during one would
        // wait for the turn to end with nothing on screen explaining the silence.
        if busy {
            return Ran::Said("the agent is working; ask again when the turn is over".into());
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
    Ran::Said(match command {
        "help" | "" => help(root),
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
        other => format!("no such command: /{other}\n{}", help(root)),
    })
}

/// The commands, the project's own listed after the built-ins.
fn help(root: &std::path::Path) -> String {
    let mine = crate::config::user_commands(root);
    if mine.is_empty() {
        return HELP.to_string();
    }
    format!(
        "{HELP}\n\nthis project also defines:\n{}",
        mine.iter()
            .map(|n| format!("/{n}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

const HELP: &str = "\
/why    why this model is the one answering
/cost   what this session has spent, in tokens and time
/context how full the window is, and what is holding it
/compact elide the oldest steps now rather than when it fills
/approve what the configuration settles without asking
/plan   read and search, change nothing
/act    leave planning, change things again
/model  what the backend serves, or /model <name> to switch and remember it
/diff   everything the agent has changed this session
/undo   put back the most recent turn that changed something
/init   write a first draft of this project's instructions
/tools  the tools this session can call, and where they came from
/mcp    the tool servers, or /mcp <server> <prompt> to take one of their prompts
/help   this, and any command the project defines";

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
    fn scrollable(content: usize, view: usize) -> UiState {
        UiState {
            content_lines: content,
            view_height: view,
            ..Default::default()
        }
    }

    /// Scrolling past the top used to leave the pane blank: the window slid off the end of
    /// the content instead of stopping at its oldest line.
    #[test]
    fn scrolling_back_stops_at_the_oldest_line() {
        let mut s = scrollable(100, 20);
        for _ in 0..50 {
            s.scroll_back(20);
        }
        assert_eq!(s.scroll, 80, "scrolled past the content");
    }

    #[test]
    fn scrolling_forward_stops_at_the_newest() {
        let mut s = scrollable(100, 20);
        s.scroll_back(40);
        s.scroll_forward(1000);
        assert_eq!(s.scroll, 0);
    }

    /// Content shorter than the window has nowhere to scroll to.
    #[test]
    fn a_short_conversation_does_not_scroll() {
        let mut s = scrollable(5, 20);
        s.scroll_back(10);
        assert_eq!(s.scroll, 0);
    }

    /// The up arrow walks back through what was sent; the down arrow returns to an empty
    /// input rather than sticking on the last entry.
    #[test]
    fn the_arrows_walk_what_was_sent() {
        let mut s = UiState::default();
        s.remember("first");
        s.remember("second");

        assert_eq!(s.older().as_deref(), Some("second"));
        assert_eq!(s.older().as_deref(), Some("first"));
        assert_eq!(
            s.older().as_deref(),
            Some("first"),
            "walked off the beginning"
        );

        assert_eq!(s.newer().as_deref(), Some("second"));
        assert_eq!(s.newer().as_deref(), Some(""));
        assert_eq!(s.newer(), None);
    }

    #[test]
    fn nothing_sent_means_nothing_to_walk() {
        let mut s = UiState::default();
        assert_eq!(s.older(), None);
        assert_eq!(s.newer(), None);
    }

    #[test]
    fn a_repeat_is_not_remembered_twice() {
        let mut s = UiState::default();
        s.remember("same");
        s.remember("same");
        assert_eq!(s.history.len(), 1);
    }

    #[test]
    fn quit_is_recognised() {
        assert!(is_quit(&ctrl('c')));
        assert!(is_quit(&ctrl('q')));
        assert!(!is_quit(&press(KeyCode::Char('c'))));
        assert_eq!(approval_key(&ctrl('c')), None);
    }
}
