use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use ratatui_textarea::TextArea;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::agent_loop::AgentEvent;
use crate::agent::protocol::ToolProtocol;
use crate::app::AppConfig;
use crate::boot;
use crate::checkpoint::Checkpoints;
use crate::sandbox::approval::ApprovalPrompt;
use crate::ui::chat::{Entry, Outcome, Step};
use crate::ui::commands::{
    booted_summary, copy_out, refresh_status, remember_rule, run_command, run_shell, take_back,
    CommandCtx, Ran,
};
use crate::ui::input::{
    accept_choice, approval_key, at_start, fresh_input, nothing_left_to_complete, open_picker,
    quit_intent, write_elsewhere, Known, Quit, ASK_AGAIN_WITHIN,
};
use crate::ui::state::{show, what_was_said, UiState};
use crate::ui::term;

/// How long to let the agent finish what it is doing after being asked to stop.
///
/// It is asked, not killed: aborting the task can drop it inside a filesystem write and
/// leave a file half-written. The grace period bounds how long a wedged tool can delay
/// the exit before the process leaves anyway.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// How far a page key moves in a list, which is what a page of it is.
const ROWS_AT_A_TIME: usize = 12;

/// Move the highlight in the list now open, if one is.
fn chosen_move(state: &mut UiState, by: isize) {
    if let Some(chooser) = state.choosing.as_mut() {
        chooser.move_by(by);
    }
}

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
        find_window,
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

    // What window the backend serves, found out with the screen already up. Asking a
    // server that holds no model means holding one ready first, which is the whole load
    // of it: a minute of blank terminal before anything was drawn, because none of this
    // used to run until it was finished.
    {
        let backend = agent.backend.clone();
        let model_asked = agent.cfg.model.clone();
        let url = cfg.backend_url.clone();
        let nearby = crate::config::backend_is_nearby(&cfg.backend_url);
        // What this session asked for, so what comes back is only remarked on when it
        // differs.
        let asked = cfg
            .models
            .get(&agent.cfg.model)
            .and_then(|m| m.context_tokens)
            .unwrap_or(cfg.context_tokens);
        let control = control_tx.clone();
        let events = agent_evt_tx.clone();
        let token = shutdown.child_token();
        tokio::spawn(async move {
            if let (true, Ok(http)) = (find_window, crate::tools::registry::tool_http_client()) {
                let mut said = crate::backend::window::of(&http, &url, &model_asked).await;
                // A server describes the models it holds, and a cold one holds none. What
                // the next request needs anyway is the model held ready, so it is asked
                // for - and said, because it is the slowest thing a session ever waits on.
                //
                // Only where the weights are near: a hosted endpoint has nothing to hold
                // ready and would bill the attempt.
                if said.is_none() && nearby && !token.is_cancelled() {
                    let _ = events
                        .send(AgentEvent::Background(Some(format!(
                            "holding {model_asked} ready on the backend"
                        ))))
                        .await;
                    let warmed = tokio::select! {
                        _ = token.cancelled() => None,
                        done = backend.warm(&model_asked) => Some(done),
                    };
                    let _ = events.send(AgentEvent::Background(None)).await;
                    // Asked again only if it was held: a backend that could not hold the
                    // model has nothing new to say about the window. The catalogue below
                    // is still worth having - it is what finishes a name being typed, and
                    // skipping it left completion silently empty.
                    if matches!(warmed, Some(Ok(()))) {
                        said = crate::backend::window::of(&http, &url, &model_asked).await;
                    }
                }
                match said {
                    Some(served) => {
                        let _ = control
                            .send(crate::agent::agent_loop::Control::SeeWindow(served))
                            .await;
                        // Said when it is not what was asked for, which is the case worth
                        // knowing: the history is about to be sized to somebody else's
                        // number rather than the one in the configuration.
                        if served != asked {
                            let _ = events
                                .send(AgentEvent::Notice(format!(
                                    "the backend serves {served} tokens for {model_asked}, \
                                     not the {asked} asked for"
                                )))
                                .await;
                        }
                    }
                    // Nothing said, so nothing is known: the window the session was
                    // configured with stands, and a prompt cut to fit a smaller one is
                    // caught from what comes back.
                    None => {
                        let _ = events
                            .send(AgentEvent::Notice(
                                "the backend does not say what window it serves; watching \
                                 what comes back instead"
                                    .into(),
                            ))
                            .await;
                    }
                }
            }
            // The catalogue, for finishing the name of a model as it is being typed.
            if let Ok(models) = backend.models().await {
                let _ = events.send(AgentEvent::Models(models)).await;
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
                    agent.control(request, &agent_evt_tx_for_loop).await;
                }
            }
        }
    });

    let startup_notices = [instruction_notes, mcp.notices.clone()].concat();
    let mcp_summary = booted_summary(&mcp);

    // The hook goes in before the terminal is taken, so the window in which a panic could
    // escape uncovered never opens.
    term::install_panic_hook();
    // Asked before the screen is taken: the answer arrives on the input the event loop
    // reads from then on.
    crate::ui::theme::set(crate::ui::theme::Theme::detect());
    // The grammars are read off the screen's thread, so the first code block does not
    // wait for them.
    tokio::task::spawn_blocking(crate::ui::highlight::warm);
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
            "lokode: the agent did not stop within {}s; exiting anyway",
            SHUTDOWN_GRACE.as_secs()
        );
    }
    result
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
    restored: Vec<crate::backend::wire::Message>,
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
        mouse_ours: true,
        focused: true,
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
    if !restored.is_empty() {
        // In view, which used to be said of messages that were nowhere on screen: the
        // model could see a conversation the person could not, and the first question of
        // the new turn read as the first question of the session.
        let said = restored.len();
        state.entries.extend(what_was_said(&restored));
        state.entries.push(Entry::Notice(format!(
            "resumed session {session_id}: {said} earlier message(s) are back"
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
        // The window's title and the tab's progress say what the session is doing, and are
        // written only when that changes.
        let project = cfg
            .root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let (doing, progress) = match (state.pending_approval.is_some(), state.busy) {
            (true, _) => (" · waiting for you", term::Progress::Waiting),
            (false, true) => (" · working", term::Progress::Busy),
            (false, false) => ("", term::Progress::Off),
        };
        let title = format!("lokode · {project}{doing}");
        if title != state.title {
            term::set_title(&title);
            state.title = title;
        }
        if state.shown_progress != Some(progress) {
            term::progress(progress);
            state.shown_progress = Some(progress);
        }
        term::begin_frame();
        let drawn =
            term.draw(|f| crate::ui::view::draw(f, &mut state, &input, &cfg.root, cfg.max_steps));
        term::end_frame();
        drawn?;

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

        // The only timer, armed only while time passing is itself a change to draw. At
        // idle every reason to redraw arrives as an event - a keystroke, a resize, a piece
        // of the answer, an approval - and this arm never completes, so nothing wakes the
        // loop.
        let wake = state.next_wake();
        let timer = async {
            match wake {
                Some(after) => tokio::time::sleep(after).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            biased;

            _ = timer => {
                state.tick = state.tick.wrapping_add(1);
                state.follow_the_drag();
            }

            Some(prompt) = approval_rx.recv() => {
                // Somewhere else, the person would not see the question until they came
                // back to look.
                if !state.focused {
                    term::notify("lokode", &format!("waiting for you: {}", prompt.req.summary));
                }
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
                state.print_line(&printed.tool, &printed.line);
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
                        state.prefill = None;
                    }
                    AgentEvent::Prefill { done, total } => state.prefill = Some((done, total)),
                    AgentEvent::Engine(engine) => state.engine = engine,
                    AgentEvent::Reasoning(p) => state.push_reasoning(&p),
                    AgentEvent::AssistantDelta(chunk) => state.write_answer(chunk),
                    AgentEvent::AssistantText(s) => state.finish_answer(s),
                    AgentEvent::ToolCall { id, name, args_pretty } => {
                        state.step += 1;
                        state
                            .entries
                            .push(Entry::Step(Step::called(id, name, &args_pretty)));
                    }
                    AgentEvent::ToolResult { id, name, output, took, origin } => {
                        // The list is the model's, so it is read back after the call that
                        // may have changed it rather than kept in step by hand.
                        if name == "update_plan" {
                            state.tasks = tasks.get().await;
                        }
                        state.settle_step(
                            &id,
                            name,
                            Outcome {
                                is_error: output.is_error,
                                body: output.content,
                                took,
                                origin,
                            },
                        );
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
                    AgentEvent::Window(window) => state.window = window,
                    AgentEvent::Rate(tokens_a_second) => state.note_rate(tokens_a_second),
                    AgentEvent::Background(doing) => {
                        state.background = doing.map(|what| (what, Instant::now()))
                    }
                    AgentEvent::Models(models) => state.models = models,
                    AgentEvent::Notice(s) => state.entries.push(Entry::Notice(s)),
                    AgentEvent::Receipt(r) => {
                        if !state.focused {
                            term::notify("lokode", &format!("turn over: {}", r.describe()));
                        }
                        state.entries.push(Entry::Receipt(r));
                    }
                    AgentEvent::TurnDone => {
                        // Asked once a turn is over, which is when the conversation has
                        // changed and the agent is free to answer.
                        refresh_status(&control_tx, checkpoints.as_deref(), &mut state).await;
                        state.busy = false;
                        state.step = 0;
                        state.phase = None;
                        state.prefill = None;
                        state.turn_cancel = None;
                        // A turn only ends while a prompt is up if it was abandoned: the
                        // tool that asked has already given up waiting, so the modal is
                        // asking about something that will not happen either way.
                        state.pending_approval = None;
                        state.waiting_since = None;
                        // What was typed while it ran and held back for now. One at a
                        // time: each is a turn, and the ones after it wait as they were.
                        if !state.queued.is_empty() {
                            let text = state.queued.remove(0);
                            state.entries.push(Entry::User(text.clone()));
                            state.remember(&text);
                            state.busy = true;
                            state.phase = Some(crate::agent::agent_loop::Phase::Preparing);
                            state.phase_since = Some(Instant::now());
                            let cancel = shutdown.child_token();
                            state.turn_cancel = Some(cancel.clone());
                            let _ = input_tx.send(TurnRequest { text, cancel }).await;
                        }
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
                match evt {
                    Event::FocusGained => {
                        state.focused = true;
                        continue;
                    }
                    Event::FocusLost => {
                        state.focused = false;
                        continue;
                    }
                    _ => {}
                }
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
                                    // Once a session, where it is about to matter: what
                                    // is being selected here is this program's, and the
                                    // terminal's copy key cannot see it.
                                    if !state.said_how_to_copy {
                                        state.said_how_to_copy = true;
                                        state.entries.push(Entry::Notice(
                                            "selecting here, not in the terminal: ctrl-y \
                                             takes this. For the terminal's own selection \
                                             and its copy key, hold shift while dragging, \
                                             or press ctrl-t to hand it the mouse"
                                                .into(),
                                        ));
                                    }
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
                            // Dragged to an edge, the pane follows. Without it a
                            // selection could never be longer than the window it is made
                            // in, which is exactly when taking a copy is worth the
                            // trouble. The edge row itself counts, not only what is past
                            // it: a pane that starts at the top of the terminal has no
                            // row above it for a pointer to be reported on.
                            state.drag_edge = if mouse.row <= state.pane.y {
                                Some(-1)
                            } else if mouse.row + 1 >= state.pane.y + state.pane.height {
                                Some(1)
                            } else {
                                None
                            };
                        }
                        // Letting go ends the drag and leaves the selection where it
                        // is. Taking it is a separate act: copying the moment a button
                        // comes up replaces whatever was on the clipboard before, which
                        // nobody asked for and which is noticed only once something
                        // wanted is gone.
                        MouseEventKind::Up(MouseButton::Left) if state.dragging => {
                            state.dragging = false;
                            state.drag_edge = None;
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
                            KeyCode::Enter | KeyCode::Down => {
                                state.finding = Some((needle.clone(), nth + 1));
                                state.point_at_match();
                                continue;
                            }
                            // Backwards, which every search has and this one did not: a
                            // word gone past could only be reached by going round.
                            KeyCode::Up => {
                                let back = match (nth, state.found) {
                                    (0, 0) => 0,
                                    (0, n) => n - 1,
                                    (nth, _) => nth - 1,
                                };
                                state.finding = Some((needle.clone(), back));
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
                        // Refused, with something to go on. A bare refusal tells a model
                        // that the call failed rather than that it was the wrong call, so
                        // the next step is usually the same call again and the person
                        // refuses it twice.
                        // Almost right, which is the common case for a shell command: a
                        // path wrong, a flag missing. Refusing it and retyping the whole
                        // line from what is on screen is the work the prompt was meant to
                        // save. This hands the line over to be corrected and run.
                        if matches!(key.code, KeyCode::Char('x') | KeyCode::Char('X')) {
                            if let Some(command) = state
                                .pending_approval
                                .as_ref()
                                .and_then(|p| p.req.summary.strip_prefix("$ "))
                                .map(str::to_string)
                            {
                                if let Some(prompt) = state.pending_approval.take() {
                                    let _ = prompt.reply.send(
                                        crate::sandbox::approval::ApprovalDecision::Deny(Some(
                                            "not as written; the user is running a corrected \
                                             version themselves and will say what it printed"
                                                .into(),
                                        )),
                                    );
                                }
                                state.waiting_since = None;
                                state.at_entry = None;
                                input = fresh_input(&format!("!{command}"));
                                continue;
                            }
                        }
                        if matches!(key.code, KeyCode::Char('e') | KeyCode::Char('E')) {
                            state.refusing = state.pending_approval.take();
                            state.waiting_since = None;
                            // The pointer goes with the question. It was left on the
                            // approval, where space unfolds what is being pointed at -
                            // so every space in the reason being typed unfolded the diff
                            // instead of reaching the line, and the model was told
                            // "neverdeleteanything".
                            state.at_entry = None;
                            state.entries.push(Entry::Notice(
                                "not doing that - say what to do instead; enter sends it, \
                                 esc refuses without a reason"
                                    .into(),
                            ));
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
                    // A list a command opened owns the keyboard. It is a question being
                    // asked of the person, and any key let through would answer a
                    // different one.
                    if key.kind == KeyEventKind::Press && state.choosing.is_some() {
                        let step = ROWS_AT_A_TIME as isize;
                        match key.code {
                            KeyCode::Esc => state.choosing = None,
                            KeyCode::Up => chosen_move(&mut state, -1),
                            KeyCode::Down => chosen_move(&mut state, 1),
                            KeyCode::PageUp => chosen_move(&mut state, -step),
                            KeyCode::PageDown => chosen_move(&mut state, step),
                            KeyCode::Backspace => {
                                if let Some(c) = state.choosing.as_mut() {
                                    c.pop();
                                }
                            }
                            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                                if let Some(chooser) = state.choosing.as_mut() {
                                    chooser.push(c);
                                }
                            }
                            KeyCode::Enter => {
                                let taken = state.choosing.as_ref().and_then(|c| {
                                    c.chosen().map(|value| (c.verb().to_string(), value))
                                });
                                state.choosing = None;
                                if let Some((verb, value)) = taken {
                                    let ran = run_command(
                                        &format!("{verb} {value}"),
                                        CommandCtx {
                                            checkpoints: checkpoints.as_deref(),
                                            root: &cfg.root,
                                            control: &control_tx,
                                            policy: &cfg.policy,
                                            tools: &tools,
                                            digest: &digest,
                                            mcp: &mcp,
                                            mcp_summary: &mcp_summary,
                                            session_id: &session_id,
                                            approvals: &session_approvals,
                                            busy: state.busy,
                                        },
                                    )
                                    .await;
                                    match ran {
                                        Ran::Said(reply) => show(&mut state, &verb, reply),
                                        // One list may lead to another: choosing a server
                                        // is not yet choosing what to ask it.
                                        Ran::Choose(chooser) => state.choosing = Some(*chooser),
                                        Ran::Cleared(said) => {
                                            state.start_again();
                                            state.entries.push(Entry::Notice(said));
                                        }
                                        Ran::Resumed(resumed) => state.show_resumed(*resumed),
                                        // Something to ask the model. Put on the line
                                        // rather than sent, so the person sees what they
                                        // are about to ask before it goes.
                                        Ran::Prompt(prompt) => input = fresh_input(&prompt),
                                    }
                                    refresh_status(&control_tx, checkpoints.as_deref(), &mut state).await;
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }

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
                                        session_id: &session_id,
                                        approvals: &session_approvals,
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
                            // A refusal being explained, which is a question still on the
                            // table: escaping it refuses without saying anything rather
                            // than leaving the turn waiting on an answer nobody is giving.
                            KeyCode::Esc if state.refusing.is_some() => {
                                if let Some(prompt) = state.refusing.take() {
                                    let _ = prompt
                                        .reply
                                        .send(crate::sandbox::approval::ApprovalDecision::Deny(
                                            None,
                                        ));
                                }
                                state.at_entry = None;
                                input = fresh_input("");
                                continue;
                            }
                            KeyCode::Enter if state.refusing.is_some() => {
                                let said = input.lines().join("\n").trim().to_string();
                                if let Some(prompt) = state.refusing.take() {
                                    let _ = prompt.reply.send(
                                        crate::sandbox::approval::ApprovalDecision::Deny(
                                            (!said.is_empty()).then_some(said.clone()),
                                        ),
                                    );
                                }
                                state.entries.push(Entry::Notice(match said.is_empty() {
                                    true => "refused".to_string(),
                                    false => format!("refused: {said}"),
                                }));
                                state.at_entry = None;
                                input = fresh_input("");
                                continue;
                            }
                            // A selection let go of, which is the first thing Esc should
                            // reach while one is on screen.
                            KeyCode::Esc if state.selecting.is_some() => {
                                state.selecting = None;
                                continue;
                            }
                            KeyCode::Esc if !state.busy && !input.is_empty() => {
                                // Kept, because clearing a line is not always meant to
                                // destroy it: the up arrow offers it back before the
                                // things that were actually sent.
                                state.draft = Some(input.lines().join("\n"));
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
                                let later = state.entries
                                    .iter()
                                    .skip(at)
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
                            // The two ends of the conversation, which had no key at all:
                            // getting back to where a long session started meant holding
                            // page-up, and getting back to now meant holding page-down.
                            KeyCode::Home => {
                                state.scroll_back(state.content_lines);
                                continue;
                            }
                            KeyCode::End => {
                                state.scroll = 0;
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
                            // What was just cleared, before what was sent: it is the more
                            // recent of the two and the one a person is reaching for.
                            KeyCode::Up if at_start(&input) && input.is_empty() && state.draft.is_some() => {
                                if let Some(draft) = state.draft.take() {
                                    input = fresh_input(&draft);
                                }
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
                                                            // A line beginning with `/` is for lokode, not the model.
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
                                    let started = Instant::now();
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
                                    state.entries.push(Entry::Step(Step::ran_by_hand(
                                        command,
                                        is_error,
                                        body,
                                        started.elapsed(),
                                    )));
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
                                            session_id: &session_id,
                                            approvals: &session_approvals,
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
                                            show(&mut state, verb, reply);
                                            refresh_status(&control_tx, checkpoints.as_deref(), &mut state).await;
                                            continue;
                                        }
                                        // A command the project defines is a way of
                                        // asking, so it becomes this turn's prompt.
                                        Ran::Prompt(prompt) => text = prompt,
                                        Ran::Choose(chooser) => {
                                            state.choosing = Some(*chooser);
                                            input = fresh_input("");
                                            continue;
                                        }
                                        // The screen is the conversation, so it goes with
                                        // it: a cleared session under a full screen of
                                        // what it no longer holds is a lie about what the
                                        // model can see.
                                        Ran::Cleared(said) => {
                                            state.start_again();
                                            state.entries.push(Entry::Notice(said));
                                            input = fresh_input("");
                                            refresh_status(&control_tx, checkpoints.as_deref(), &mut state).await;
                                            continue;
                                        }
                                        Ran::Resumed(resumed) => {
                                            state.show_resumed(*resumed);
                                            input = fresh_input("");
                                            refresh_status(&control_tx, checkpoints.as_deref(), &mut state).await;
                                            continue;
                                        }
                                    }
                                }
                                // Typed while a turn runs and sent with the modifier: it
                                // waits for the turn to end rather than joining it. What a
                                // person types mid-turn is as often the next thing to do
                                // as a correction to this one, and steering the model with
                                // the next thing sends it off before it has finished.
                                if state.busy && key.modifiers.contains(KeyModifiers::ALT) {
                                    state.entries.push(Entry::Notice(format!(
                                        "queued for when this turn is over: {}",
                                        text.lines().next().unwrap_or_default()
                                    )));
                                    state.queued.push(text);
                                    continue;
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
                                state.rate = None;
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
                    // Handing the mouse to the terminal, and taking it back. A terminal
                    // whose own selection is turned off has nothing for its own copy key
                    // to copy, and that key - ctrl-shift-c on most - never reaches a
                    // program to be reassigned. So the mouse is lent back on request.
                    if key.code == KeyCode::Char('t') && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        state.mouse_ours = !state.mouse_ours;
                        state.selecting = None;
                        state.dragging = false;
                        state.drag_edge = None;
                        let _ = crate::ui::term::capture_mouse(state.mouse_ours);
                        state.entries.push(Entry::Notice(match state.mouse_ours {
                            false => "the terminal has the mouse: drag to select and copy \
                                      with the terminal's own key, usually ctrl-shift-c. \
                                      The wheel scrolls the terminal, not the \
                                      conversation. ctrl-t takes it back"
                                .into(),
                            true => "the mouse is back: the wheel scrolls the conversation \
                                     and dragging selects here, ctrl-y takes what is \
                                     selected"
                                .to_string(),
                        }));
                        continue;
                    }
                    // Taking what is selected, when the person says so. The terminal's
                    // own copy key never reaches a program, so a pane that does its own
                    // selecting has to offer its own way to take it.
                    if key.code == KeyCode::Char('y') && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        // What is selected, or else what is being pointed at: taking one
                        // answer out of a conversation is the common case, and dragging
                        // over exactly its lines to do it is work the keyboard can save.
                        let taking = state.selection_text().or_else(|| {
                            state
                                .at_entry
                                .and_then(|at| state.entries.get(at))
                                .map(|e| e.to_text())
                        });
                        let said = match taking {
                            Some(text) => match crate::ui::clipboard::put(&text) {
                                Ok(()) => format!("{} line(s) taken", text.lines().count()),
                                // A terminal that ignores the sequence says so to nobody,
                                // so this does.
                                Err(e) => format!("could not reach the clipboard: {e}"),
                            },
                            None => "nothing is selected".to_string(),
                        };
                        // On the status line rather than at the foot of the conversation:
                        // a selection long enough to be worth taking was made by
                        // scrolling, and what is appended down there is not on screen.
                        state.said = Some((said, Instant::now()));
                        continue;
                    }
                    // A long instruction written where writing is comfortable. The line
                    // grows to a few rows and then scrolls, which is right for a sentence
                    // and wrong for a specification: the editor a person already knows is
                    // a better place to write thirty lines than a box six rows tall.
                    if key.code == KeyCode::Char('g') && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        let carried = input.lines().join("\n");
                        match write_elsewhere(term, &carried).await {
                            Ok(Some(text)) => input = fresh_input(&text),
                            // Left empty or unchanged: the editor was opened and thought
                            // better of, which is not a failure to report.
                            Ok(None) => {}
                            Err(e) => state
                                .entries
                                .push(Entry::Notice(format!("could not open an editor: {e}"))),
                        }
                        continue;
                    }
                    // The rail, over a narrow screen or out of the way of a wide one.
                    if key.code == KeyCode::Char('b') && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        state.rail = Some(!state.rail_now);
                        continue;
                    }
                    if key.code == KeyCode::Char('f') && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        state.finding = Some((String::new(), 0));
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
                    state.picker = open_picker(
                        &input,
                        state.picker.take(),
                        &cfg.root,
                        &Known {
                            models: &state.models,
                            mcp: &mcp,
                        },
                    );
                }
            }
        }
    }
}
