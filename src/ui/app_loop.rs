use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
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
        session_approvals,
        mut agent,
        lsp,
        mut approval_rx,
        shutdown,
        recorder,
        session_id,
        model,
        protocol,
        protocol_cell,
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
        let cell = protocol_cell.clone();
        tokio::spawn(async move {
            if crate::backend::probe::ignores_native_tools(backend, &model_probed, token).await {
                cell.set(crate::agent::protocol::ToolProtocol::Envelope);
                let _ = tx
                    .send(crate::agent::agent_loop::Control::Protocol(
                        crate::agent::protocol::ToolProtocol::Envelope,
                    ))
                    .await;
                let _ = notices
                    .send(AgentEvent::Protocol(
                        crate::agent::protocol::ToolProtocol::Envelope,
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
        session_approvals,
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
    /// Where a drag with the mouse began and where it is now, in lines of the wrapped
    /// pane. Both ends are kept because the person may drag in either direction.
    selecting: Option<(crate::ui::chat::Spot, crate::ui::chat::Spot)>,
    /// When quitting was last asked for and not confirmed. A session holds work nobody
    /// else has - what was said, what was changed, what can still be put back - so leaving
    /// it is asked for twice.
    quit_asked: Option<Instant>,
    /// Whether writes go ahead unasked, for this stretch of work.
    accepting_writes: bool,
    /// What is being looked for in the conversation, and which match is shown.
    ///
    /// A long session holds what a tool answered an hour ago, and the only way back to it
    /// was the wheel. The match is pointed at rather than extracted, so what is found is
    /// read in the place it happened.
    finding: Option<(String, usize)>,
    /// Which wrapped line each entry begins at, as the pane was last drawn.
    entry_line: Vec<usize>,
    /// When Esc was last pressed with nothing for it to do. Pressed twice it takes the
    /// last question back, which is the shortcut for the commonest correction there is.
    esc_at: Option<Instant>,
    /// Whether the button is still down: a drag that has been let go stays on screen,
    /// so it can be seen and so a second look does not have to be a second drag.
    dragging: bool,
    /// The pane as it was last drawn, wrapped to its width. What the mouse points at is a
    /// row of this, and nothing else can say which line a row holds.
    wrapped: Vec<Line<'static>>,
    /// Where the pane sat on screen when it was last drawn.
    pane: ratatui::layout::Rect,
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
    /// How tool calls travel, as the agent has it: settled by asking the backend, so the
    /// interface can only know it by asking.
    protocol: crate::agent::protocol::ToolProtocol,
    /// When the approval now on screen was asked for. `None` when nothing is waiting.
    waiting_since: Option<Instant>,
    /// The list open over the input, when the token being typed has one.
    picker: Option<crate::ui::picker::Picker>,
    /// A command's answer, on a page over the conversation until it is closed.
    sheet: Option<crate::ui::sheet::Sheet>,
    /// The entry the keys are pointing at, when the user has moved off the newest.
    ///
    /// `None` means the bottom of the conversation, where a session spends its time; a
    /// mark is only worth drawing once somebody has gone looking for something.
    at_entry: Option<usize>,
    /// Entries shown whole rather than counted.
    unfolded: std::collections::HashSet<usize>,
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
    /// Scroll so that an entry is on screen, when it is not already.
    ///
    /// Put at the top rather than centred: what a person wants to read starts there and
    /// runs downward, and centring hides the half of it they were looking for.
    fn show_entry(&mut self, at: usize) {
        let Some(&line) = self.entry_line.get(at) else {
            return;
        };
        let height = self.pane.height as usize;
        let first = self.first_visible();
        if line >= first && line < first + height {
            return;
        }
        let from_bottom = self
            .content_lines
            .saturating_sub(height)
            .saturating_sub(line);
        self.scroll = from_bottom.min(u16::MAX as usize) as u16;
    }

    /// Point at the match being looked for, counting from the most recent.
    ///
    /// From the most recent because that is where a person is: what was said a moment ago
    /// is what they are usually trying to find again, and walking forward from the start
    /// of a long session would make them press the key a hundred times to get there.
    fn point_at_match(&mut self) {
        let Some((needle, nth)) = self.finding.clone() else {
            return;
        };
        if needle.is_empty() {
            self.at_entry = None;
            return;
        }
        let needle = needle.to_lowercase();
        let found: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, e)| e.to_text().to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect();
        if found.is_empty() {
            self.at_entry = None;
            return;
        }
        // Round rather than stop: walking past the last match comes back to the first,
        // which is what every search in a terminal does.
        let at = found[nth % found.len()];
        self.at_entry = Some(at);
        self.show_entry(at);
    }

    /// Which line and column a point on screen holds, or nothing when it is not in the
    /// conversation.
    ///
    /// The pane scrolls, so a row on screen is a row of what is visible rather than of
    /// what there is; the first visible line is what turns one into the other.
    fn spot_at(&self, column: u16, row: u16) -> Option<crate::ui::chat::Spot> {
        let pane = self.pane;
        if row < pane.y || row >= pane.y + pane.height || column < pane.x {
            return None;
        }
        let first = self.first_visible();
        let line = first + (row - pane.y) as usize;
        if line >= self.wrapped.len() {
            return None;
        }
        let width = crate::ui::chat::line_width(&self.wrapped[line]);
        Some(crate::ui::chat::Spot {
            line,
            column: ((column - pane.x) as usize).min(width),
        })
    }

    /// Where a drag has reached, pinned inside the conversation.
    ///
    /// A drag that leaves the pane is still a drag: letting go outside it must take what
    /// was crossed rather than nothing, which is what a strict answer here would give.
    fn spot_at_clamped(&self, column: u16, row: u16) -> crate::ui::chat::Spot {
        if let Some(spot) = self.spot_at(column, row) {
            return spot;
        }
        let last = self.wrapped.len().saturating_sub(1);
        let first = self.first_visible();
        let line = match row < self.pane.y {
            true => first,
            false => (first + self.pane.height.saturating_sub(1) as usize).min(last),
        };
        let width = self
            .wrapped
            .get(line)
            .map(crate::ui::chat::line_width)
            .unwrap_or(0);
        crate::ui::chat::Spot {
            line,
            column: width,
        }
    }

    /// The first line of the conversation now on screen.
    fn first_visible(&self) -> usize {
        let height = self.pane.height as usize;
        let end = self.content_lines.saturating_sub(self.scroll as usize);
        end.saturating_sub(height)
    }

    /// What the selection holds, or nothing when it is empty.
    fn selection_text(&self) -> Option<String> {
        let (a, b) = self.selecting?;
        let text = crate::ui::chat::selected_text(&self.wrapped, a, b);
        (!text.trim().is_empty()).then_some(text)
    }

    /// Move towards older lines, stopping where they run out.
    fn scroll_back(&mut self, by: usize) {
        let max = self.content_lines.saturating_sub(self.view_height);
        self.scroll = (self.scroll as usize).saturating_add(by).min(max) as u16;
    }

    fn scroll_forward(&mut self, by: usize) {
        self.scroll = (self.scroll as usize).saturating_sub(by) as u16;
    }

    /// Point at the entry before the one being pointed at, or at the newest.
    ///
    /// Only the entries worth stopping on: pointing at every blank remark between two
    /// steps makes the key useless for finding the step.
    fn point_back(&mut self) {
        let before = self.at_entry.unwrap_or(self.entries.len());
        self.at_entry = (0..before)
            .rev()
            .find(|i| worth_pointing_at(&self.entries[*i]));
        // Brought into view, since pointing at something off the top of the pane is
        // pointing at nothing.
        if let Some(at) = self.at_entry {
            self.show_entry(at);
        }
    }

    fn point_forward(&mut self) {
        let Some(after) = self.at_entry else { return };
        self.at_entry =
            (after + 1..self.entries.len()).find(|i| worth_pointing_at(&self.entries[*i]));
        if let Some(at) = self.at_entry {
            self.show_entry(at);
        }
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

/// An input holding `text`, with the cursor after it.
///
/// After it, because everything that puts text here puts it there to be carried on with:
/// a recalled line to be edited, a command just completed to be given an argument. A
/// cursor left at the start makes the next keystroke insert in front of what is there and
/// backspace do nothing at all.
fn fresh_input(text: &str) -> TextArea<'static> {
    let mut input = TextArea::from(text.lines().map(String::from).collect::<Vec<_>>());
    input.move_cursor(tui_textarea::CursorMove::End);
    input.move_cursor(tui_textarea::CursorMove::Bottom);
    input.move_cursor(tui_textarea::CursorMove::End);
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
    // What has been agreed to for this session, so a standing decision can be taken from
    // the keyboard as well as at the prompt.
    session_approvals: std::sync::Arc<crate::sandbox::approval::SessionApprovals>,
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
        protocol,
        ..Default::default()
    };
    // The protocol is not stated here: it is settled by asking the backend, which has not
    // answered yet, and the status line carries whatever it turns out to be.
    // The keys live on their own line under the input, where they are always in view, so
    // the welcome says the one thing that line cannot: what this session is talking to.
    state
        .entries
        .push(Entry::Notice(format!("backend {}", cfg.backend_url)));
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

    let mut events = EventStream::new();

    loop {
        term.draw(|f| {
            // No borders anywhere. They cost four columns and two lines, they land in
            // every selection made with the mouse, and they separate nothing that a blank
            // line and a prompt mark do not separate better.
            //
            // The input grows with what is being typed, up to a few lines, so a long
            // instruction is visible while it is written instead of scrolling inside a box
            // of fixed height.
            let typed = input.lines().len().clamp(1, INPUT_MAX_LINES) as u16;
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(3),
                    Constraint::Length(1),
                    Constraint::Length(typed),
                    Constraint::Length(1),
                    Constraint::Length(1),
                ])
                .split(f.area());

            // Wrapped entry by entry, so where each one begins is known. Pointing at a
            // step said nothing about where it is: the mark went on an entry that could be
            // a screen and a half above the pane, and the only way to see it was to guess
            // how far to scroll.
            let mut lines: Vec<Line<'static>> = Vec::new();
            let mut starts: Vec<usize> = Vec::with_capacity(state.entries.len());
            for (i, e) in state.entries.iter().enumerate() {
                starts.push(lines.len());
                lines.extend(crate::ui::chat::wrap(
                    e.render(state.unfolded.contains(&i), state.at_entry == Some(i)),
                    chunks[0].width as usize,
                ));
            }
            state.entry_line = starts;
            // Wrapped above rather than by the widget. The program counted logical lines
            // while the terminal showed rows, so scrolling was off by however much had
            // wrapped and nothing could say which line sits at a given row - which is what
            // pointing at text with the mouse needs to know.
            state.pane = chunks[0];
            let inner_h = chunks[0].height as usize;
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
            let chosen = state
                .selecting
                .map(|(a, b)| if a <= b { (a, b) } else { (b, a) });
            let visible: Vec<Line<'static>> = lines[start..end]
                .iter()
                .enumerate()
                .map(|(row, line)| {
                    let row = start + row;
                    match chosen {
                        Some((from, to)) if row >= from.line && row <= to.line => {
                            let a = if row == from.line { from.column } else { 0 };
                            let b = if row == to.line {
                                to.column
                            } else {
                                crate::ui::chat::line_width(line)
                            };
                            crate::ui::chat::mark_selected(line, a, b)
                        }
                        _ => line.clone(),
                    }
                })
                .collect();
            f.render_widget(Paragraph::new(visible), chunks[0]);
            // Kept for the mouse, which can only say which row was pointed at.
            state.wrapped = lines;

            // The prompt mark, in its own column, so the text the user types starts where
            // the answers do and a wrapped line does not sit under the mark.
            let typing = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(2), Constraint::Min(1)])
                .split(chunks[2]);
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "❯ ",
                    Style::default()
                        .fg(if state.busy {
                            Color::DarkGray
                        } else {
                            Color::Cyan
                        })
                        .add_modifier(Modifier::BOLD),
                ))),
                typing[0],
            );
            f.render_widget(&input, typing[1]);
            if let Some(picker) = state.picker.as_ref() {
                picker.render(f, typing[1]);
            }

            let waiting = state.pending_approval.is_some();
            let status = Status {
                model: state.model.clone(),
                // The last component: it is what names the project, and the rest is where
                // it happens to sit on this disk.
                root: cfg
                    .root
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| cfg.root.display().to_string()),
                planning: state.planning,
                pointing: state.at_entry.is_some(),
                context_percent: state.context_percent,
                step: state.step,
                max_steps: cfg.max_steps,
                busy: state.busy,
                typing: !input.is_empty(),
                accepting_writes: state.accepting_writes,
                phase: if waiting {
                    Some("waiting for your answer".to_string())
                } else {
                    state.phase.as_ref().map(|p| p.label())
                },
                in_phase: if waiting {
                    state.waiting_since.map(|t| t.elapsed()).unwrap_or_default()
                } else {
                    state.phase_since.map(|t| t.elapsed()).unwrap_or_default()
                },
                tick: state.tick,
            };
            f.render_widget(
                Paragraph::new(status.to_line()).style(Style::default().fg(Color::Gray)),
                chunks[3],
            );
            f.render_widget(Paragraph::new(status.keys()), chunks[4]);

            if let Some(sheet) = state.sheet.as_mut() {
                sheet.render(f, chunks[0]);
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
                // In the thread, where the conversation the change has to be judged
                // against is still visible, and where it scrolls with everything else.
                state.entries.push(Entry::Approval {
                    tool: prompt.req.tool.clone(),
                    summary: prompt.req.summary.clone(),
                    diff: prompt.req.diff_preview.clone(),
                    scope: prompt.req.scope.describe_under(&cfg.root),
                });
                // Pointing at it, so space opens the rest of the diff without anyone
                // having to go looking for it.
                state.at_entry = Some(state.entries.len() - 1);
                state.pending_approval = Some(prompt);
                // The turn is not running anything: it is waiting on the person. Saying
                // "running write_file" for the minutes a prompt sits there is the one
                // thing the status line must never do.
                state.waiting_since = Some(Instant::now());
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
                    AgentEvent::Protocol(protocol) => {
                        state.protocol = protocol;
                        state.entries.push(Entry::Notice(format!(
                            "this backend does not use the tool definitions it accepts; \
                             tool calls now travel as the {} instead",
                            protocol.label()
                        )));
                    }
                    AgentEvent::Window(percent) => state.context_percent = percent,
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
                        state.waiting_since = None;
                    }
                }
            }

            maybe_evt = events.next() => {
                let Some(Ok(evt)) = maybe_evt else { continue };
                // Mouse capture was turned on from the beginning and nothing ever read an
                // event, so the wheel did nothing in a pane whose purpose is scrolling.
                // Pasted text arrives whole, so the newlines in it are newlines. Handed
                // over a keystroke at a time - which is what a terminal does when nobody
                // has asked otherwise - every one of them was the send key, and pasting
                // ten lines asked ten questions made of fragments.
                if let Event::Paste(text) = evt {
                    let text = text.replace("\r\n", "\n").replace('\r', "\n");
                    let mut lines = text.split('\n');
                    if let Some(first) = lines.next() {
                        input.insert_str(first);
                    }
                    for line in lines {
                        input.insert_newline();
                        input.insert_str(line);
                    }
                    continue;
                }
                if let Event::Mouse(mouse) = evt {
                    match mouse.kind {
                        MouseEventKind::ScrollUp => state.scroll_back(3),
                        MouseEventKind::ScrollDown => state.scroll_forward(3),
                        // Taking the mouse is what buys the wheel, and it is also what
                        // takes the terminal's own drag-to-select away. So the pane does
                        // the selecting itself: pressing starts one, moving extends it,
                        // and letting go puts it on the clipboard. Both are had at once
                        // rather than one being traded for the other.
                        MouseEventKind::Down(MouseButton::Left) => {
                            match state.spot_at(mouse.column, mouse.row) {
                                Some(spot) => {
                                    state.selecting = Some((spot, spot));
                                    state.dragging = true;
                                }
                                // Pressed outside the conversation: the previous selection
                                // is let go, which is how a selection is dismissed.
                                None => state.selecting = None,
                            }
                        }
                        MouseEventKind::Drag(MouseButton::Left) if state.dragging => {
                            if let Some((from, _)) = state.selecting {
                                let to = state.spot_at_clamped(mouse.column, mouse.row);
                                state.selecting = Some((from, to));
                            }
                        }
                        MouseEventKind::Up(MouseButton::Left) if state.dragging => {
                            state.dragging = false;
                            if let Some(text) = state.selection_text() {
                                let said = match crate::ui::clipboard::put(&text) {
                                    Ok(()) => format!(
                                        "{} line(s) copied",
                                        text.lines().count()
                                    ),
                                    // A terminal that ignores the sequence says so to
                                    // nobody, so this does.
                                    Err(e) => format!("could not reach the clipboard: {e}"),
                                };
                                state.entries.push(Entry::Notice(said));
                                state.scroll = 0;
                            }
                        }
                        _ => {}
                    }
                    continue;
                }
                if let Event::Key(key) = evt {
                    // Leaving is tested before anything else can swallow the key. It used
                    // to sit after the approval branch, which returned early - so the one
                    // screen the user most wants to escape from was the one screen where
                    // Ctrl-C did nothing.
                    //
                    // What Ctrl-C means depends on what is in front of it, which is what
                    // everyone who has used a terminal expects: stop what is running,
                    // then clear what is typed, and only on an empty line does it offer to
                    // leave - and then only when asked twice. It used to end the session
                    // outright, in the middle of a turn, with a change history nobody had
                    // looked at yet.
                    match quit_intent(&key) {
                        Some(Quit::Now) => return Ok(()),
                        Some(Quit::OnEmptyLine) if input.is_empty() => return Ok(()),
                        Some(Quit::OnEmptyLine) => {}
                        Some(Quit::Ask) => {
                            if state.busy {
                                if let Some(cancel) = state.turn_cancel.take() {
                                    cancel.cancel();
                                    state.entries.push(Entry::Notice("interrupting…".into()));
                                }
                            } else if !input.is_empty() {
                                // The emptied line is the answer; saying so as well would
                                // be narrating what is already on screen.
                                input = fresh_input("");
                                state.quit_asked = None;
                            } else if state
                                .quit_asked
                                .is_some_and(|at| at.elapsed() < ASK_AGAIN_WITHIN)
                            {
                                return Ok(());
                            } else {
                                state.quit_asked = Some(Instant::now());
                                state.entries.push(Entry::Notice(
                                    "press ctrl-c again to leave, or ctrl-d".into(),
                                ));
                            }
                            continue;
                        }
                        None => {
                            // Any other key withdraws the offer: a person who carried on
                            // working did not mean to leave.
                            state.quit_asked = None;
                        }
                    }

                    // A search owns the keyboard while it is open: enter walks the
                    // matches, which is what enter means in every search, and the send
                    // key had it first.
                    if let Some((needle, nth)) = state.finding.clone() {
                        match key.code {
                            KeyCode::Esc => {
                                state.finding = None;
                                continue;
                            }
                            KeyCode::Enter => {
                                state.finding = Some((needle.clone(), nth + 1));
                                state.point_at_match();
                                continue;
                            }
                            KeyCode::Backspace => {
                                let mut needle = needle.clone();
                                needle.pop();
                                state.finding = Some((needle, 0));
                                state.point_at_match();
                                continue;
                            }
                            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                                state.finding = Some((format!("{needle}{c}"), 0));
                                state.point_at_match();
                                continue;
                            }
                            _ => {}
                        }
                    }

                    if state.pending_approval.is_some() {
                        // Space still opens the rest of the diff: the question is in the
                        // thread now, and what is under it is what it is about.
                        if key.code == KeyCode::Char(' ') {
                            if let Some(i) = state.at_entry {
                                if !state.unfolded.remove(&i) {
                                    state.unfolded.insert(i);
                                }
                            }
                            continue;
                        }
                        if let Some(decision) = approval_key(&key) {
                            if let Some(prompt) = state.pending_approval.take() {
                                let _ = prompt.reply.send(decision);
                            }
                            state.waiting_since = None;
                            state.at_entry = None;
                            // The question is answered; what is left is what was done.
                            if let Some(i) = state
                                .entries
                                .iter()
                                .rposition(|e| matches!(e, Entry::Approval { .. }))
                            {
                                state.entries.remove(i);
                                state.unfolded.remove(&i);
                            }
                        }
                        continue;
                    }

                    // A page over the conversation takes the keys until it is closed: it
                    // is the only thing on screen and anything else would act on what it
                    // is covering.
                    if key.kind == KeyEventKind::Press && state.sheet.is_some() {
                        let height = state.view_height.saturating_sub(2);
                        match key.code {
                            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                                state.sheet = None;
                            }
                            KeyCode::Up => {
                                if let Some(s) = state.sheet.as_mut() {
                                    s.scroll(-1, height);
                                }
                            }
                            KeyCode::Down => {
                                if let Some(s) = state.sheet.as_mut() {
                                    s.scroll(1, height);
                                }
                            }
                            KeyCode::PageUp => {
                                if let Some(s) = state.sheet.as_mut() {
                                    s.scroll(-(height as isize), height);
                                }
                            }
                            KeyCode::PageDown => {
                                if let Some(s) = state.sheet.as_mut() {
                                    s.scroll(height as isize, height);
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // The list over the input takes the keys that move and choose, and
                    // nothing else: everything typed still reaches the input, which is
                    // what narrows the list.
                    if key.kind == KeyEventKind::Press && state.picker.is_some() {
                        let done = match key.code {
                            KeyCode::Esc => {
                                state.picker = None;
                                true
                            }
                            KeyCode::Up => {
                                if let Some(p) = state.picker.as_mut() {
                                    p.move_by(-1);
                                }
                                true
                            }
                            KeyCode::Down => {
                                if let Some(p) = state.picker.as_mut() {
                                    p.move_by(1);
                                }
                                true
                            }
                            // Tab always completes. Enter completes only while there is
                            // something left to complete: a command typed in full and
                            // matched exactly is finished, and a key that refuses to send
                            // it is a key that has to be pressed twice for no reason.
                            KeyCode::Tab => {
                                if let Some(text) = accept_choice(&input, state.picker.as_ref()) {
                                    input = fresh_input(&text);
                                }
                                state.picker = None;
                                true
                            }
                            KeyCode::Enter if !nothing_left_to_complete(&input, &state.picker) => {
                                if let Some(text) = accept_choice(&input, state.picker.as_ref()) {
                                    input = fresh_input(&text);
                                }
                                state.picker = None;
                                true
                            }
                            KeyCode::Enter => {
                                state.picker = None;
                                false
                            }
                            _ => false,
                        };
                        if done {
                            continue;
                        }
                    }

                    if key.kind == KeyEventKind::Press {
                        match key.code {
                            // The one setting a person changes several times an hour, on
                            // the key every agent puts it on. A command for it exists and
                            // nobody finds a command they have to read about first.
                            //
                            // Three settings rather than two: reading without changing
                            // anything, changing with every change shown first, and a
                            // stretch of editing where the changes go through. The third
                            // was only reachable by writing a file or by answering the
                            // prompt over and over, which is the thing it exists to stop.
                            KeyCode::BackTab | KeyCode::Tab
                                if key.modifiers.contains(KeyModifiers::SHIFT)
                                    && !state.busy
                                    && !state.planning
                                    && !state.accepting_writes =>
                            {
                                state.accepting_writes = true;
                                session_approvals.accept_writes(true);
                                state.entries.push(Entry::Notice(
                                    "writing without asking: changes go through, commands \
                                     and anything leaving the machine are still asked about"
                                        .into(),
                                ));
                                continue;
                            }
                            KeyCode::BackTab
                            | KeyCode::Tab
                                if key.modifiers.contains(KeyModifiers::SHIFT)
                                    && !state.busy =>
                            {
                                let said = run_command(
                                    if state.planning { "act" } else { "plan" },
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
                                if let Ran::Said(text) = said {
                                    state.planning = !state.planning;
                                    state.entries.push(Entry::Notice(text));
                                }
                                // Leaving the stretch of editing, whichever way the toggle
                                // went: a standing decision that outlived the work it was
                                // taken for is a decision nobody remembers taking.
                                state.accepting_writes = false;
                                session_approvals.accept_writes(false);
                                continue;
                            }
                            // Nothing to interrupt, nothing open, not pointing: the one
                            // thing left for Esc to mean is the line being typed. It did
                            // nothing at all there, which is a key that works everywhere
                            // except where it was pressed.
                            KeyCode::Esc if !state.busy && !input.is_empty() => {
                                input = fresh_input("");
                                state.esc_at = None;
                                continue;
                            }
                            // Twice over, on a line with nothing on it: the last question
                            // comes back to be asked differently. Pointing at it and
                            // pressing enter does the same thing to any question; this is
                            // the one everybody wants, on the key the habit is already on.
                            KeyCode::Esc if !state.busy => {
                                if state
                                    .esc_at
                                    .is_some_and(|at| at.elapsed() < ASK_AGAIN_WITHIN)
                                {
                                    state.esc_at = None;
                                    match take_back(&control_tx).await {
                                        Some(text) => {
                                            let at = state
                                                .entries
                                                .iter()
                                                .rposition(|e| matches!(e, Entry::User(_)));
                                            if let Some(at) = at {
                                                state.entries.truncate(at);
                                            }
                                            input = fresh_input(&text);
                                        }
                                        None => state.entries.push(Entry::Notice(
                                            "nothing has been asked yet".into(),
                                        )),
                                    }
                                } else {
                                    state.esc_at = Some(Instant::now());
                                }
                                continue;
                            }
                            KeyCode::Esc if state.busy => {
                                if let Some(cancel) = state.turn_cancel.take() {
                                    cancel.cancel();
                                    state.entries.push(Entry::Notice("interrupting…".into()));
                                }
                                continue;
                            }
                            // Between the entries rather than between the lines: what a
                            // person is looking for is a step, not a row.
                            KeyCode::Up if key.modifiers.contains(KeyModifiers::ALT) => {
                                state.point_back();
                                continue;
                            }
                            KeyCode::Down if key.modifiers.contains(KeyModifiers::ALT) => {
                                state.point_forward();
                                continue;
                            }
                            // A question taken back to be asked differently. Getting a
                            // word wrong is the commonest way a turn goes astray, and the
                            // only answer to it was to ask again from scratch and leave
                            // the failed attempt in the window for the model to read.
                            //
                            // What follows it leaves the conversation, so the next attempt
                            // starts where the first one did rather than after it.
                            KeyCode::Enter
                                if !state.busy
                                    && state
                                        .at_entry
                                        .is_some_and(|i| matches!(state.entries.get(i), Some(Entry::User(_)))) =>
                            {
                                let at = state.at_entry.expect("checked just above");
                                let later = state.entries[at..]
                                    .iter()
                                    .filter(|e| matches!(e, Entry::User(_)))
                                    .count();
                                let mut taken = None;
                                for _ in 0..later {
                                    taken = take_back(&control_tx).await;
                                }
                                match taken {
                                    Some(text) => {
                                        state.entries.truncate(at);
                                        state.at_entry = None;
                                        input = fresh_input(&text);
                                    }
                                    None => state.entries.push(Entry::Notice(
                                        "that question is no longer in the conversation".into(),
                                    )),
                                }
                                continue;
                            }
                            // What was counted, shown whole, and back again.
                            KeyCode::Char(' ') if state.at_entry.is_some() => {
                                if let Some(i) = state.at_entry {
                                    if !state.unfolded.remove(&i) {
                                        state.unfolded.insert(i);
                                    }
                                }
                                continue;
                            }
                            KeyCode::Esc if state.at_entry.is_some() && !state.busy => {
                                state.at_entry = None;
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
                            // Answered here rather than in the command table: it is the
                            // one command whose subject is the pane itself.
                            KeyCode::Enter
                                if !key.modifiers.contains(KeyModifiers::SHIFT)
                                    && input.lines().join("\n").trim().starts_with("/copy") =>
                            {
                                let what = input.lines().join("\n");
                                let last = what.trim() == "/copy last";
                                input = fresh_input("");
                                let said = copy_out(&state.entries, last, &cfg.root);
                                state.entries.push(Entry::Notice(said));
                                continue;
                            }
                            KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                                let text: String = input.lines().join("\n");
                                if text.trim().is_empty() { continue; }
                                state.entries.push(Entry::User(text.clone()));
                                input = TextArea::default();
                                                            // A line beginning with `/` is for verve, not the model.
                                // Handled before the turn is started so that `/undo` works
                                // when the thing you want undone is what the model just did.
                                state.remember(&text);
                                // A line beginning with a hash is a rule for this
                                // project, written down where the next session will read
                                // it. Deciding it in conversation and having to open an
                                // editor to keep it is how a convention stays in one
                                // person's head.
                                if let Some(rule) = text.strip_prefix('#') {
                                    let rule = rule.trim().to_string();
                                    if rule.is_empty() {
                                        continue;
                                    }
                                    let said = remember_rule(&rule, &cfg.root);
                                    // Told to the model now as well: a rule that only
                                    // applies from the next session is a rule that does
                                    // not apply to the work it was written during.
                                    let _ = control_tx
                                        .send(crate::agent::agent_loop::Control::Note(format!(
                                            "A standing rule for this project, from now on: \
                                             {rule}"
                                        )))
                                        .await;
                                    state.entries.push(Entry::Notice(said));
                                    continue;
                                }
                                // A line beginning with an exclamation is the person's own
                                // command, run here and now. Asking the model to run it
                                // costs a round trip, a share of the window and an
                                // approval, for a decision that was already taken by
                                // typing it. Its output joins the conversation, so the
                                // model sees what you saw on the next question.
                                if let Some(command) = text.strip_prefix('!') {
                                    let command = command.trim().to_string();
                                    if command.is_empty() {
                                        continue;
                                    }
                                    let (is_error, body) =
                                        run_shell(
                                            &command,
                                            &cfg.root,
                                            crate::agent::session::tool_output_chars(
                                                cfg.context_tokens,
                                                cfg.max_answer_tokens,
                                            ),
                                        )
                                        .await;
                                    // Handed to the conversation as well: running a test
                                    // and then asking about the failure only works if the
                                    // failure is something the model has seen.
                                    let _ = control_tx
                                        .send(crate::agent::agent_loop::Control::Note(format!(
                                            "I ran `{command}` myself. It said:\n{body}"
                                        )))
                                        .await;
                                    state.entries.push(Entry::ToolResult {
                                        name: command,
                                        is_error,
                                        body,
                                    });
                                    continue;
                                }
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
                                            // A sentence belongs in the thread, where it
                                            // reads as what just happened. A page of it
                                            // does not: it is read once, it is not part of
                                            // the conversation, and left there it sits
                                            // between two questions for the rest of the
                                            // session.
                                            match reply.lines().count() > SHEET_LINES {
                                                true => {
                                                    state.sheet = Some(
                                                        crate::ui::sheet::Sheet::new(
                                                            format!("/{verb}"),
                                                            &reply,
                                                        ),
                                                    )
                                                }
                                                false => {
                                                    state.entries.push(Entry::Notice(reply))
                                                }
                                            }
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
                    if key.code == KeyCode::Char('f') && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        state.finding = Some((String::new(), 0));
                        state.entries.push(Entry::Notice(
                            "looking: type, enter for the next, esc to stop".into(),
                        ));
                        continue;
                    }
                    // Erasing to the start of the line, as it does in every shell. The
                    // editor underneath binds it to undo, so a habit that clears a line
                    // put back the word the last one had removed.
                    if key.code == KeyCode::Char('u') && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        input.delete_line_by_head();
                        continue;
                    }
                    input.input(key);
                    // Opened, narrowed or closed by what is now on the line, rather than
                    // by the key that was pressed: the two cannot disagree that way.
                    state.picker = open_picker(&input, state.picker.take(), &cfg.root);
                }
            }

            _ = tokio::time::sleep(Duration::from_millis(80)) => {}
        }
    }
}

/// How long an offer to leave stands before it has to be made again.
///
/// Long enough to be a deliberate second press, short enough that a Ctrl-C typed minutes
/// apart is two interruptions rather than an exit.
const ASK_AGAIN_WITHIN: Duration = Duration::from_secs(3);

/// What a key says about leaving.
enum Quit {
    /// Stop what is running, clear what is typed, or offer to leave - in that order,
    /// because that is the order of what is in front of the person.
    Ask,
    /// The end-of-input key: on an empty line it means what it has always meant.
    OnEmptyLine,
    /// Asked for in a way that leaves no doubt.
    Now,
}

fn quit_intent(k: &KeyEvent) -> Option<Quit> {
    if k.kind != KeyEventKind::Press || !k.modifiers.contains(KeyModifiers::CONTROL) {
        return None;
    }
    match k.code {
        KeyCode::Char('c') => Some(Quit::Ask),
        KeyCode::Char('d') => Some(Quit::OnEmptyLine),
        KeyCode::Char('q') => Some(Quit::Now),
        _ => None,
    }
}

/// Add one rule to what this project tells the agent, and say where it went.
///
/// Appended rather than rewritten: the file is the team's, it is in version control with
/// the code it is about, and a tool that reorganises it would be a tool nobody lets near
/// it twice.
fn remember_rule(rule: &str, root: &std::path::Path) -> String {
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
async fn take_back(control: &mpsc::Sender<crate::agent::agent_loop::Control>) -> Option<String> {
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
async fn run_shell(command: &str, root: &std::path::Path, cap: usize) -> (bool, String) {
    let done = tokio::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .current_dir(root)
        .output()
        .await;
    let out = match done {
        Ok(out) => out,
        Err(e) => return (true, format!("could not run it: {e}")),
    };
    let mut body = String::from_utf8_lossy(&out.stdout).into_owned();
    let errors = String::from_utf8_lossy(&out.stderr);
    if !errors.trim().is_empty() {
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(&errors);
    }
    if body.trim().is_empty() {
        body = match out.status.success() {
            true => "(said nothing)".into(),
            false => format!("(said nothing, {})", out.status),
        };
    }
    if body.len() > cap {
        let cut = (0..=cap)
            .rev()
            .find(|&i| body.is_char_boundary(i))
            .unwrap_or(0);
        body.truncate(cut);
        body.push_str("\n… (truncated to fit the pane)");
    }
    (!out.status.success(), body)
}

/// Put the conversation where a person can take it: on the terminal's clipboard, and in a
/// file that is always there whether or not the terminal took it.
///
/// Both, because neither can be relied on alone. A terminal either understands the copy
/// sequence or ignores it without saying so, and a file is no use to somebody who wanted
/// to paste into a message.
fn copy_out(entries: &[Entry], last_only: bool, root: &std::path::Path) -> String {
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

/// The list the line being typed calls for, carrying over the one already open.
///
/// Rebuilt from the line rather than driven by keystrokes, so the list and the text can
/// never disagree about what is being completed.
fn open_picker(
    input: &TextArea,
    open: Option<crate::ui::picker::Picker>,
    root: &std::path::Path,
) -> Option<crate::ui::picker::Picker> {
    let line = input.lines().join("\n");
    let (sigil, _, token) = crate::ui::picker::token_at(&line)?;
    let mut picker = match open {
        Some(p) if p.sigil() == sigil => p,
        _ => crate::ui::picker::Picker::new(
            sigil,
            match sigil {
                '/' => command_choices(root),
                _ => path_choices(root),
            },
        ),
    };
    picker.narrow(&token);
    (!picker.is_empty()).then_some(picker)
}

/// Whether what is typed already is what is highlighted.
///
/// A command typed in full and matched exactly has nothing left to complete, so the key
/// that sends should send rather than replace the line with itself.
fn nothing_left_to_complete(input: &TextArea, picker: &Option<crate::ui::picker::Picker>) -> bool {
    let Some(picker) = picker else { return true };
    let line = input.lines().join("\n");
    let Some((_, _, token)) = crate::ui::picker::token_at(&line) else {
        return true;
    };
    picker.chosen().is_some_and(|c| c == token)
}

/// The line with the token being completed replaced by what was chosen.
fn accept_choice(input: &TextArea, picker: Option<&crate::ui::picker::Picker>) -> Option<String> {
    let line = input.lines().join("\n");
    let chosen = picker?.chosen()?;
    let (sigil, at, _) = crate::ui::picker::token_at(&line)?;
    // A command is the whole line and is ready to send; a file is one word in a sentence
    // and the sentence carries on after it.
    Some(match sigil {
        '/' => format!("/{chosen}"),
        _ => format!("{}@{chosen}", &line[..at]),
    })
}

/// Whether an entry is one somebody would go looking for.
///
/// A question, an answer, and a step: the things a session is made of. Not the remarks
/// between them, which are read as they go by and never sought out.
fn worth_pointing_at(e: &Entry) -> bool {
    matches!(
        e,
        Entry::User(_)
            | Entry::Assistant(_)
            | Entry::AssistantStreaming(_)
            | Entry::ToolCall { .. }
            | Entry::ToolResult { .. }
    )
}

/// How long an answer may be before it goes on a page of its own rather than into the
/// conversation.
///
/// Three lines is a sentence and its detail, which reads as something that just happened.
/// More than that is a document, and a document in the thread is in the way of everything
/// after it.
const SHEET_LINES: usize = 3;

/// How tall the input may grow before it scrolls inside itself.
const INPUT_MAX_LINES: usize = 6;

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
    if let Ok(report) = rx.await {
        state.model = report.model;
        state.context_percent = report.context_percent;
        state.protocol = report.protocol;
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

/// The commands, as a list to choose from.
///
/// Read off the same text the help prints, so a command cannot be offered by one and
/// missing from the other.
fn command_choices(root: &std::path::Path) -> Vec<crate::ui::picker::Choice> {
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

/// The files and directories under the root, for naming one with `@`.
///
/// Bounded and gitignore-aware, like everything else that walks the project: a list is for
/// choosing from, and a list of every file in a large repository is for scrolling through.
fn path_choices(root: &std::path::Path) -> Vec<crate::ui::picker::Choice> {
    const MOST: usize = 400;
    let mut out = Vec::new();
    for entry in ignore::WalkBuilder::new(root)
        .git_ignore(true)
        .require_git(false)
        .hidden(true)
        .build()
        .flatten()
    {
        if entry.depth() == 0 {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        out.push(crate::ui::picker::Choice::new(
            match is_dir {
                true => format!("{}/", rel.display()),
                false => rel.display().to_string(),
            },
            "",
        ));
        if out.len() >= MOST {
            break;
        }
    }
    out.sort_by(|a, b| a.value.cmp(&b.value));
    out
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
/retry  ask the last question again, or /retry <model> to ask another model
/copy   put the conversation on the clipboard and in a file, or /copy last for the answer
/diff   everything the agent has changed this session
/undo   put back the most recent turn that changed something
/digest what has been worked out about this project, or /digest forget
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
    /// A row on screen is a line of the conversation, and which line depends on where the
    /// pane has been scrolled to.
    ///
    /// This is where a selection goes wrong without looking wrong: the highlight follows
    /// the mouse, the text taken comes from somewhere else, and only reading the clipboard
    /// afterwards shows it. So the mapping is pinned at the top of the pane, at its bottom,
    /// scrolled back, and outside it.
    #[test]
    fn a_row_on_screen_is_the_line_the_pane_shows_there() {
        let mut state = UiState {
            wrapped: (0..100)
                .map(|i| Line::from(format!("line {i} with some text")))
                .collect(),
            pane: ratatui::layout::Rect::new(0, 0, 40, 10),
            ..Default::default()
        };
        state.content_lines = state.wrapped.len();

        // At the foot of the conversation the last ten lines are shown, so the top row is
        // line 90 and the bottom row is line 99.
        assert_eq!(state.spot_at(0, 0).unwrap().line, 90);
        assert_eq!(state.spot_at(0, 9).unwrap().line, 99);
        // Scrolled back by five, everything moves with it.
        state.scroll = 5;
        assert_eq!(state.spot_at(0, 0).unwrap().line, 85);
        assert_eq!(state.spot_at(0, 9).unwrap().line, 94);
        // The column is where the pointer is, never past the end of what the line holds.
        assert_eq!(state.spot_at(6, 0).unwrap().column, 6);
        assert_eq!(
            state.spot_at(39, 0).unwrap().column,
            crate::ui::chat::line_width(&state.wrapped[85])
        );
        // Below the pane is not the conversation.
        assert!(state.spot_at(0, 10).is_none());
        // A drag that leaves the pane still reaches the row it left by.
        assert_eq!(state.spot_at_clamped(0, 30).line, 94);
        assert_eq!(state.spot_at_clamped(0, 0).line, 85);
    }

    /// An empty drag is not a selection: pressing without moving must not replace the
    /// clipboard with nothing.
    #[test]
    fn a_press_that_selected_nothing_takes_nothing() {
        let mut state = UiState {
            wrapped: vec![Line::from("some text")],
            pane: ratatui::layout::Rect::new(0, 0, 40, 1),
            ..Default::default()
        };
        state.content_lines = 1;
        let spot = crate::ui::chat::Spot { line: 0, column: 4 };
        state.selecting = Some((spot, spot));
        assert!(state.selection_text().is_none());
        state.selecting = Some((crate::ui::chat::Spot { line: 0, column: 0 }, spot));
        assert_eq!(state.selection_text().as_deref(), Some("some"));
    }

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

    /// What each of the leaving keys means, and what an ordinary key does not mean.
    ///
    /// Ctrl-C asks rather than ends: it used to end a session outright, in the middle of a
    /// turn, taking with it a change history nobody had looked at. Ctrl-D is the
    /// end-of-input key and means what it means everywhere, on an empty line. Ctrl-Q is
    /// the one that leaves no doubt.
    #[test]
    fn the_keys_that_leave_are_told_apart() {
        assert!(matches!(quit_intent(&ctrl('c')), Some(Quit::Ask)));
        assert!(matches!(quit_intent(&ctrl('d')), Some(Quit::OnEmptyLine)));
        assert!(matches!(quit_intent(&ctrl('q')), Some(Quit::Now)));
        // A letter is a letter.
        assert!(quit_intent(&press(KeyCode::Char('c'))).is_none());
        assert!(quit_intent(&press(KeyCode::Char('d'))).is_none());
        assert_eq!(approval_key(&ctrl('c')), None);
    }
}
