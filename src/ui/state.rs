//! What the interface holds between frames: the conversation as drawn, where it is
//! scrolled to, what is selected and what has been typed before.

use std::time::{Duration, Instant};

use ratatui::text::Line;
use tokio_util::sync::CancellationToken;

use crate::sandbox::approval::ApprovalPrompt;
use crate::ui::chat::{Entry, Outcome, Step};
use crate::ui::thread::{Drawings, Pane, Thread};

/// A conversation as entries, for putting an earlier session back on screen.
///
/// What a person said and what the model answered.
/// Tool calls and what they printed are left out.
pub(crate) fn what_was_said(messages: &[crate::backend::wire::Message]) -> Vec<Entry> {
    messages
        .iter()
        .filter_map(|m| match m.role.as_str() {
            "user" => Some(Entry::User(m.content.clone())),
            "assistant" if !m.content.trim().is_empty() => {
                Some(Entry::Assistant(m.content.clone()))
            }
            _ => None,
        })
        .collect()
}

/// Put what a command said where it belongs.
///
/// A sentence belongs in the thread, where it reads as what just happened.
/// A page of it goes on a sheet: it is read once and is not part of the conversation.
pub(crate) fn show(state: &mut UiState, verb: &str, reply: String) {
    use crate::ui::sheet::{Body, Sheet};
    // The window is drawn as the map the interface already holds.
    if verb == "context" && state.window.budget > 0 {
        state.sheet = Some(Sheet::with(verb, Body::Window(state.window.clone())));
        return;
    }
    match reply.lines().count() > SHEET_LINES {
        true => state.sheet = Some(Sheet::new(verb, &reply)),
        false => state.entries.push(Entry::Notice(reply)),
    }
}

/// Something running: what it is, for how long, its step and what it is on inside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Job {
    pub what: String,
    pub took: Duration,
    /// The entry of its step, when it has one.
    pub entry: Option<usize>,
    /// The call a delegated session is on.
    pub doing: Option<String>,
    /// Its place among the work aside, when it is some.
    pub aside: Option<usize>,
    /// Whether it can be stopped alone.
    pub stoppable: bool,
    /// Whether it is a language server at work.
    pub lsp: bool,
}

/// What an item of the list of running things leads to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JobTarget {
    /// The step at this entry, to point at.
    Step(usize),
    /// Work aside, to stop.
    Aside(AsideKind),
    /// A language server at work, whose page opens.
    Lsp,
}

/// Work done for the session outside any turn.
#[derive(Debug, Clone)]
pub(crate) struct Aside {
    pub kind: AsideKind,
    pub what: String,
    pub since: Instant,
    pub stop: tokio_util::sync::CancellationToken,
}

/// Which work aside it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AsideKind {
    /// The model held ready on the backend.
    Warm,
    /// A question about the project looked into.
    Discover,
    /// An agent working in the background, by its number.
    Agent(u64),
}

/// What a click lands on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Click {
    /// A key, as a badge stands for it.
    Key(char),
    /// The entry at this index, to unfold or fold.
    Unfold(usize),
}

/// How long the status line keeps saying what was just done.
pub(crate) const SAID_FOR: Duration = Duration::from_secs(4);

/// How often the spinner moves and the phase clock counts, while there is work to show.
const TICK_EVERY: Duration = Duration::from_millis(120);

/// How often a drag held past an edge scrolls the pane one more line.
const DRAG_EVERY: Duration = Duration::from_millis(80);

/// A tool server's form being answered: the field being asked, and what was given.
pub(crate) struct Asking {
    pub(crate) question: crate::mcp::host::Question,
    pub(crate) at: usize,
    pub(crate) got: serde_json::Map<String, serde_json::Value>,
}

impl UiState {
    /// Take a server's form: say what it asks, then ask its first field.
    pub(crate) fn ask(&mut self, question: crate::mcp::host::Question) {
        self.entries.push(Entry::Notice(format!(
            "{} asks: {}   (enter to answer each field, esc to decline)",
            question.server, question.message
        )));
        self.asking = Some(Asking {
            question,
            at: 0,
            got: Default::default(),
        });
        self.waiting_since = Some(Instant::now());
        self.ask_field();
    }

    /// Ask the field the form is at, or send the form once every field is answered.
    fn ask_field(&mut self) {
        let Some(asking) = &self.asking else { return };
        let fields = &asking.question.fields;
        if let Some(field) = fields.get(asking.at) {
            let line = format!("{}/{} {}", asking.at + 1, fields.len(), field.prompt());
            self.entries.push(Entry::Notice(line));
            return;
        }
        if let Some(asking) = self.asking.take() {
            self.entries.push(Entry::Notice(format!(
                "answered {}",
                asking.question.server
            )));
            let _ = asking
                .question
                .reply
                .send(crate::mcp::host::Answer::Accept(asking.got));
        }
        self.waiting_since = None;
    }

    /// What was typed for the field being asked: kept and the next one asked, or said
    /// not to fit and asked again.
    pub(crate) fn answer_field(&mut self, typed: &str) {
        let Some(asking) = &mut self.asking else {
            return;
        };
        let Some(field) = asking.question.fields.get(asking.at) else {
            return;
        };
        match field.read(typed) {
            Ok(value) => {
                if let Some(value) = value {
                    asking.got.insert(field.name.clone(), value);
                }
                asking.at += 1;
                self.ask_field();
            }
            Err(why) => self.entries.push(Entry::Notice(why)),
        }
    }

    /// Turn the form down.
    pub(crate) fn decline_form(&mut self) {
        if let Some(asking) = self.asking.take() {
            self.entries.push(Entry::Notice(format!(
                "declined what {} asked",
                asking.question.server
            )));
            let _ = asking
                .question
                .reply
                .send(crate::mcp::host::Answer::Decline);
        }
        self.waiting_since = None;
    }
}

#[derive(Default)]
pub(crate) struct UiState {
    pub(crate) entries: Thread,
    /// What each entry was last drawn as, so a frame draws only the entries that changed.
    pub(crate) drawings: Drawings,
    pub(crate) scroll: u16,
    pub(crate) pending_approval: Option<ApprovalPrompt>,
    /// Where a drag with the mouse began and where it is now, in lines of the wrapped pane.
    ///
    /// Both ends are kept: a drag goes in either direction.
    pub(crate) selecting: Option<(crate::ui::chat::Spot, crate::ui::chat::Spot)>,
    /// When quitting was last asked for and not confirmed.
    ///
    /// A session holds work nobody else has: what was said, what was changed, what can be put back.
    /// Leaving it is asked for twice.
    pub(crate) quit_asked: Option<(Instant, &'static str)>,
    /// Whether writes go ahead unasked, for this stretch of work.
    pub(crate) accepting_writes: bool,
    /// What is being looked for in the conversation, and which match is shown.
    ///
    /// The match is pointed at, so what is found is read in the place it happened.
    pub(crate) finding: Option<(String, usize)>,
    /// When Esc was last pressed with nothing for it to do.
    ///
    /// Pressed twice, it takes the last question back.
    pub(crate) esc_at: Option<Instant>,
    /// Which edge a drag is being held past, if any.
    ///
    /// The pane scrolls that way while it is held.
    /// A terminal reports a pointer that moves, not one held still.
    pub(crate) drag_edge: Option<i8>,
    /// What was just done and when, for the status line to say while it is still news.
    pub(crate) said: Option<(String, Instant)>,
    /// A list a command opened, waiting to be chosen from.
    pub(crate) choosing: Option<crate::ui::chooser::Chooser>,
    /// A call being refused, while what to do instead is being typed.
    pub(crate) refusing: Option<crate::sandbox::approval::ApprovalPrompt>,
    /// A tool server's form, while its fields are being answered.
    pub(crate) asking: Option<Asking>,
    /// What was typed during a turn to be asked once it is over.
    pub(crate) queued: Vec<String>,
    /// How many entries the search matched, for saying which one is being shown.
    pub(crate) found: usize,
    /// How fast the last step came back, in tokens a second.
    pub(crate) rate: Option<f64>,
    /// A line cleared off the input, kept for the up arrow to offer back.
    pub(crate) draft: Option<String>,
    /// What is being done for this session outside any turn.
    pub(crate) aside: Vec<Aside>,
    /// Whether the person stopped looking into the project for this session.
    pub(crate) discovery_stopped: bool,
    /// What stops each call of the turn alone, by the id of its call.
    pub(crate) stops: std::collections::HashMap<String, tokio_util::sync::CancellationToken>,
    /// Where each language server stands.
    pub(crate) lsp: Vec<crate::lsp::Status>,
    /// What the list of running things offered, in its order, while it is open.
    pub(crate) jobs_offered: Vec<JobTarget>,
    /// Where the rail was drawn and, for each of its first rows, the running thing on it.
    pub(crate) rail_hit: Option<(ratatui::layout::Rect, Vec<Option<usize>>)>,
    /// Whether the mouse belongs to this program or to the terminal.
    ///
    /// Taking it gives the wheel and a selection that can scroll past the edge of the pane.
    /// It takes away the terminal's own selection and its copy key.
    pub(crate) mouse_ours: bool,
    /// Whether the 2 ways of copying have been said this session.
    pub(crate) said_how_to_copy: bool,
    /// What the backend serves, for finishing the name of one that is being typed.
    ///
    /// Asked for once and after a switch.
    pub(crate) models: Vec<String>,
    /// Whether the button is still down.
    ///
    /// A drag that has been let go stays on screen.
    pub(crate) dragging: bool,
    /// The pane as it was last drawn, wrapped to its width.
    ///
    /// What the mouse points at is a row of this.
    pub(crate) wrapped: Pane,
    /// Where the pane sat on screen when it was last drawn.
    pub(crate) pane: ratatui::layout::Rect,
    pub(crate) busy: bool,
    pub(crate) step: usize,
    /// What the turn is doing and since when, for the status line. `None` between turns.
    pub(crate) phase: Option<crate::agent::agent_loop::Phase>,
    /// Read only while a phase is set, so where it starts costs nothing.
    pub(crate) phase_since: Option<Instant>,
    /// Advances on every repaint while busy, which is what moves the spinner.
    pub(crate) tick: usize,
    /// Whether the agent is planning.
    ///
    /// The mode lives with the agent, and the interface learns of a change only by asking.
    pub(crate) planning: bool,
    /// How full the window is and with what, as the agent last measured it.
    pub(crate) window: crate::agent::session::Occupancy,
    /// The speeds of the latest steps, oldest first, for the rail's sparkline.
    pub(crate) rates: std::collections::VecDeque<f64>,
    /// The files this session changed, as the change history last said.
    pub(crate) changes: Vec<crate::ui::rail::FileChange>,
    /// The turns that can be put back, newest first.
    pub(crate) turns: Vec<crate::checkpoint::Turn>,
    /// Whether the rail is shown: `None` follows the width of the screen, and a key sets
    /// it either way.
    pub(crate) rail: Option<bool>,
    /// How far the server has read the prompt, in tokens, while it says.
    pub(crate) prefill: Option<(u64, u64)>,
    /// What the backend measured, as of the latest step.
    pub(crate) engine: crate::agent::agent_loop::Engine,
    /// The power the backend's machine drew at the latest reading.
    pub(crate) power: Option<crate::backend::devices::Power>,
    /// The latest readings of that power, in watts, oldest first, for the sparkline.
    pub(crate) watts: std::collections::VecDeque<f64>,
    /// The project's branch and how many files differ from it, when it is a repository.
    pub(crate) git: Option<(String, usize)>,
    /// The backend's address, for the band at the top.
    pub(crate) backend: String,
    /// The project's root, for asking git where it stands.
    pub(crate) root: std::path::PathBuf,
    /// The commands of the person's own still running: the id of each one's step and what
    /// stops it, oldest first.
    pub(crate) hand: Vec<(String, tokio_util::sync::CancellationToken)>,
    /// Where the open list was drawn and the first item on screen, for the mouse.
    pub(crate) list_hit: Option<(ratatui::layout::Rect, usize)>,
    /// Whether the terminal window has the focus, as it last said.
    pub(crate) focused: bool,
    /// The window title as it was last set.
    pub(crate) title: String,
    /// The progress the tab was last told, so it is written only when it changes.
    pub(crate) shown_progress: Option<crate::ui::term::Progress>,
    /// Whether the rail was on screen in the last frame, for the key that flips it.
    pub(crate) rail_now: bool,
    /// How tool calls travel, as the agent has it.
    ///
    /// Settled by asking the backend, so the interface knows it only by asking.
    pub(crate) protocol: crate::agent::protocol::ToolProtocol,
    /// When the approval now on screen was asked for. `None` when nothing is waiting.
    pub(crate) waiting_since: Option<Instant>,
    /// The list open over the input, when the token being typed has one.
    pub(crate) picker: Option<crate::ui::picker::Picker>,
    /// A command's answer, on a page over the conversation until it is closed.
    pub(crate) sheet: Option<crate::ui::sheet::Sheet>,
    /// The entry the keys are pointing at, when the user has moved off the newest.
    ///
    /// `None` means the bottom of the conversation.
    pub(crate) at_entry: Option<usize>,
    /// Entries shown whole rather than counted.
    pub(crate) unfolded: std::collections::HashSet<usize>,
    /// The task list as it was last drawn.
    pub(crate) tasks: Vec<crate::tools::plan::Task>,
    /// The model answering now.
    ///
    /// It can change under the interface (command, mode, failed check), so it is asked for.
    pub(crate) model: String,
    /// Abandons the turn in flight. `None` between turns.
    pub(crate) turn_cancel: Option<CancellationToken>,
    /// What the chat pane last drew, so scrolling can be bounded by what exists.
    ///
    /// Known only after a draw: the height comes from the terminal and the wrapping from its width.
    pub(crate) content_lines: usize,
    pub(crate) view_height: usize,
    /// What has been sent this session, oldest first, for the up arrow.
    pub(crate) history: Vec<String>,
    /// Where in that history the arrows are. `None` means at the live input.
    pub(crate) history_at: Option<usize>,
}

impl UiState {
    /// When the screen next changes with nothing happening, or `None` when it does not.
    ///
    /// A turn or background work moves the spinner and the clocks beside it.
    /// A drag held past an edge keeps scrolling.
    /// A remark on the status line goes when its time is up.
    pub(crate) fn next_wake(&self) -> Option<Duration> {
        let mut wake = None::<Duration>;
        let mut sooner = |after: Duration| {
            wake = Some(wake.map_or(after, |w| w.min(after)));
        };
        if self.busy || !self.aside.is_empty() {
            sooner(TICK_EVERY);
        }
        if self.drag_edge.is_some() {
            sooner(DRAG_EVERY);
        }
        if let Some((_, at)) = &self.said {
            if let Some(left) = SAID_FOR.checked_sub(at.elapsed()) {
                sooner(left);
            }
        }
        wake
    }

    /// Scroll so that an entry is on screen, when it is not already.
    ///
    /// It is put at the top: what a person wants to read starts there and runs downward.
    pub(crate) fn show_entry(&mut self, at: usize) {
        let Some(line) = self.wrapped.start_of(at) else {
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
    pub(crate) fn point_at_match(&mut self) {
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
        self.found = found.len();
        if found.is_empty() {
            self.at_entry = None;
            return;
        }
        // Walking past the last match comes back to the first.
        let at = found[nth % found.len()];
        self.at_entry = Some(at);
        self.show_entry(at);
    }

    /// Which line and column a point on screen holds, or nothing when it is not in the
    /// conversation.
    ///
    /// The pane scrolls, so a row on screen is a row of what is visible.
    /// The first visible line turns it into a line of the whole conversation.
    pub(crate) fn spot_at(&self, column: u16, row: u16) -> Option<crate::ui::chat::Spot> {
        let pane = self.pane;
        if row < pane.y || row >= pane.y + pane.height || column < pane.x {
            return None;
        }
        let first = self.first_visible();
        let line = first + (row - pane.y) as usize;
        let width = crate::ui::chat::line_width(self.wrapped.get(line)?);
        Some(crate::ui::chat::Spot {
            line,
            column: ((column - pane.x) as usize).min(width),
        })
    }

    /// Where a drag has reached, pinned inside the conversation.
    ///
    /// A drag that leaves the pane is still a drag.
    /// Letting go outside it takes what was crossed.
    pub(crate) fn spot_at_clamped(&self, column: u16, row: u16) -> crate::ui::chat::Spot {
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
    pub(crate) fn first_visible(&self) -> usize {
        let height = self.pane.height as usize;
        let end = self.content_lines.saturating_sub(self.scroll as usize);
        end.saturating_sub(height)
    }

    /// Keep scrolling while a drag is held past an edge, and take the selection with it.
    ///
    /// A terminal reports a pointer that moves and says nothing once it is held still.
    /// This keeps the selection growing while the pointer is still.
    pub(crate) fn follow_the_drag(&mut self) {
        let Some(edge) = self.drag_edge else { return };
        let Some((from, _)) = self.selecting else {
            return;
        };
        match edge {
            -1 => self.scroll_back(1),
            _ => self.scroll_forward(1),
        }
        // The far end goes to the line now at that edge.
        // What is highlighted is what has been dragged over.
        let first = self.first_visible();
        let line = match edge {
            -1 => first,
            _ => (first + self.pane.height.saturating_sub(1) as usize)
                .min(self.wrapped.len().saturating_sub(1)),
        };
        let column = match edge {
            -1 => 0,
            _ => self
                .wrapped
                .get(line)
                .map(crate::ui::chat::line_width)
                .unwrap_or(0),
        };
        self.selecting = Some((from, crate::ui::chat::Spot { line, column }));
    }

    /// What the selection holds, or nothing when it is empty.
    pub(crate) fn selection_text(&self) -> Option<String> {
        let (a, b) = self.selecting?;
        // Only the rows the selection spans, counted from the first of them.
        let (from, to) = if a <= b { (a, b) } else { (b, a) };
        let rows: Vec<Line<'static>> = self.wrapped.rows(from.line, to.line + 1).cloned().collect();
        let shift = |s: crate::ui::chat::Spot| crate::ui::chat::Spot {
            line: s.line - from.line,
            column: s.column,
        };
        let text = crate::ui::chat::selected_text(&rows, shift(from), shift(to));
        (!text.trim().is_empty()).then_some(text)
    }

    /// Move towards older lines, stopping where they run out.
    pub(crate) fn scroll_back(&mut self, by: usize) {
        let max = self.content_lines.saturating_sub(self.view_height);
        self.scroll = (self.scroll as usize).saturating_add(by).min(max) as u16;
    }

    pub(crate) fn scroll_forward(&mut self, by: usize) {
        self.scroll = (self.scroll as usize).saturating_sub(by) as u16;
    }

    /// Point at the entry before the one being pointed at, or at the newest.
    ///
    /// Only the entries worth stopping on count: blank remarks between 2 steps are skipped.
    pub(crate) fn point_back(&mut self) {
        let before = self.at_entry.unwrap_or(self.entries.len());
        self.at_entry = (0..before)
            .rev()
            .find(|i| worth_pointing_at(&self.entries[*i]));
        // Brought into view: an entry off the top of the pane cannot be pointed at.
        if let Some(at) = self.at_entry {
            self.show_entry(at);
        }
    }

    pub(crate) fn point_forward(&mut self) {
        let Some(after) = self.at_entry else { return };
        self.at_entry =
            (after + 1..self.entries.len()).find(|i| worth_pointing_at(&self.entries[*i]));
        if let Some(at) = self.at_entry {
            self.show_entry(at);
        }
    }

    /// Where the answer now being written sits, if one is.
    ///
    /// Searched for, not assumed to be last.
    /// A notice, a tool line or a budget change can arrive while a model writes.
    /// An answer with one of those under it is still the answer being written.
    pub(crate) fn answer_being_written(&self) -> Option<usize> {
        self.entries
            .iter()
            .rposition(|e| matches!(e, Entry::AssistantStreaming(_)))
    }

    /// Where the newest step of `tool` still running sits, if one is.
    pub(crate) fn step_running(&self, tool: &str) -> Option<usize> {
        self.entries
            .iter()
            .rposition(|e| matches!(e, Entry::Step(s) if s.name == tool && s.outcome.is_none()))
    }

    /// The turn of the receipt being pointed at, when it changed files.
    pub(crate) fn receipt_here(&self) -> Option<u32> {
        match self.at_entry.and_then(|i| self.entries.get(i)) {
            Some(Entry::Receipt(r)) if !r.files.is_empty() => Some(r.turn),
            _ => None,
        }
    }

    /// What is running now, oldest first.
    ///
    /// Every step without an outcome, a delegated session with the call it is on, then the
    /// background work.
    pub(crate) fn jobs(&self) -> Vec<Job> {
        let mut out: Vec<Job> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| match e {
                Entry::Step(step) if step.outcome.is_none() => Some(Job {
                    what: match step.subject.is_empty() {
                        true => step.name.clone(),
                        false => format!("{}  {}", step.name, step.subject),
                    },
                    took: step.started.map(|at| at.elapsed()).unwrap_or_default(),
                    entry: Some(i),
                    doing: step
                        .children
                        .iter()
                        .rev()
                        .find(|c| c.outcome.is_none())
                        .map(|c| format!("{}  {}", c.name, c.subject)),
                    aside: None,
                    stoppable: self.stop_for(&step.id).is_some(),
                    lsp: false,
                }),
                _ => None,
            })
            .collect();
        for status in &self.lsp {
            if let crate::lsp::State::Working { what, since } = &status.state {
                out.push(Job {
                    what: format!("{}  {what}", status.command),
                    took: since.elapsed(),
                    entry: None,
                    doing: None,
                    aside: None,
                    stoppable: false,
                    lsp: true,
                });
            }
        }
        for (i, aside) in self.aside.iter().enumerate() {
            out.push(Job {
                what: aside.what.clone(),
                took: aside.since.elapsed(),
                entry: None,
                doing: None,
                aside: Some(i),
                stoppable: true,
                lsp: false,
            });
        }
        out
    }

    /// Open the list of running things, the highlight on the `at`th.
    ///
    /// False when nothing runs.
    pub(crate) fn open_jobs(&mut self, at: Option<usize>) -> bool {
        let jobs = self.jobs();
        if jobs.is_empty() {
            return false;
        }
        self.jobs_offered = jobs
            .iter()
            .map(|job| match (job.entry, job.aside) {
                (Some(entry), _) => JobTarget::Step(entry),
                (None, Some(i)) => JobTarget::Aside(self.aside[i].kind),
                (None, None) => JobTarget::Lsp,
            })
            .collect();
        let choices = jobs
            .iter()
            .enumerate()
            .map(|(i, job)| {
                let took = match job.took >= crate::ui::chat::SHOWN_FROM {
                    true => format!("  {}", crate::ui::chat::how_long(job.took)),
                    false => String::new(),
                };
                let does = match (job.aside, job.lsp) {
                    (Some(_), _) => "  enter stops it",
                    (None, true) => "  enter shows the servers",
                    (None, false) => "",
                };
                crate::ui::chooser::Choice::new(
                    (i + 1).to_string(),
                    format!("{}{took}{does}", job.what),
                )
            })
            .collect();
        let mut chooser = crate::ui::chooser::Chooser::new("jobs", "what runs", choices, None);
        if let Some(at) = at {
            chooser.set_at(at);
        }
        self.choosing = Some(chooser);
        true
    }

    /// The page of the language servers: where each stands, the files it reports problems
    /// in, and what a server that is down said.
    pub(crate) fn lsp_page(&self) -> crate::ui::sheet::Sheet {
        use crate::lsp::State;
        let mut md = String::new();
        if self.lsp.is_empty() {
            md.push_str("No language server answers for this project.\n");
        } else {
            md.push_str(
                "| Language | Server | State | Errors | Warnings |\n|---|---|---|---|---|\n",
            );
            for s in &self.lsp {
                let state = match &s.state {
                    State::Idle => "not started".to_string(),
                    State::Ready => "ready".to_string(),
                    State::Working { what, .. } => what.clone(),
                    State::Down(_) => "down".to_string(),
                    State::Missing => "not installed".to_string(),
                };
                let errors: usize = s.files.iter().map(|f| f.1).sum();
                let warnings: usize = s.files.iter().map(|f| f.2).sum();
                md.push_str(&format!(
                    "| {} | `{}` | {state} | {errors} | {warnings} |\n",
                    s.language, s.command
                ));
            }
        }
        for s in &self.lsp {
            if !s.files.is_empty() {
                md.push_str(&format!("\n### What {} reports\n\n", s.command));
                md.push_str("| File | Errors | Warnings |\n|---|---|---|\n");
                for (file, errors, warnings) in &s.files {
                    md.push_str(&format!("| {file} | {errors} | {warnings} |\n"));
                }
            }
            if let State::Down(why) = &s.state {
                md.push_str(&format!(
                    "\n### Why {} is down\n\n```\n{why}\n```\n",
                    s.command
                ));
            }
        }
        crate::ui::sheet::Sheet::titled(
            "/lsp",
            "the language servers",
            crate::ui::sheet::Body::Markdown(md),
        )
    }

    /// The errors each language server reports, by file relative to the project.
    pub(crate) fn lsp_errors(&self) -> Vec<(String, usize)> {
        self.lsp
            .iter()
            .flat_map(|s| s.files.iter())
            .filter(|f| f.1 > 0)
            .map(|f| (f.0.clone(), f.1))
            .collect()
    }

    /// Act on the `n`th item the list of running things offered: point at its step, or
    /// stop the work aside.
    pub(crate) fn take_job(&mut self, n: usize) {
        let offered = std::mem::take(&mut self.jobs_offered);
        match n.checked_sub(1).and_then(|i| offered.get(i)) {
            Some(JobTarget::Step(at)) if *at < self.entries.len() => {
                self.at_entry = Some(*at);
                self.show_entry(*at);
            }
            Some(JobTarget::Aside(kind)) => self.stop_aside(*kind),
            Some(JobTarget::Lsp) => self.sheet = Some(self.lsp_page()),
            _ => {}
        }
    }

    /// What stops the call whose step has the id `id`, while it runs.
    pub(crate) fn stop_for(&self, id: &str) -> Option<tokio_util::sync::CancellationToken> {
        self.stops.get(id).cloned().or_else(|| {
            self.hand
                .iter()
                .find(|(running, _)| running == id)
                .map(|(_, stop)| stop.clone())
        })
    }

    /// What stops the step pointed at, when it is running and can be stopped.
    pub(crate) fn pointed_stop(&self) -> Option<tokio_util::sync::CancellationToken> {
        match self.at_entry.and_then(|i| self.entries.get(i)) {
            Some(Entry::Step(step)) if step.outcome.is_none() => self.stop_for(&step.id),
            _ => None,
        }
    }

    /// Stop the work aside of `kind`.
    ///
    /// Looking into the project stays stopped for the session.
    pub(crate) fn stop_aside(&mut self, kind: AsideKind) {
        if let Some(at) = self.aside.iter().position(|a| a.kind == kind) {
            let aside = self.aside.remove(at);
            aside.stop.cancel();
            if kind == AsideKind::Discover {
                self.discovery_stopped = true;
            }
            self.said = Some((format!("stopped: {}", aside.what), Instant::now()));
        }
    }

    /// Show the entry at `i` whole if it was counted, or counted if it was whole.
    pub(crate) fn toggle_unfolded(&mut self, i: usize) {
        if !self.unfolded.remove(&i) {
            self.unfolded.insert(i);
        }
    }

    /// What a click without a drag at `spot` does.
    ///
    /// A badge of the question waiting is its key.
    /// A row of an entry with more to show unfolds or folds it.
    pub(crate) fn clicked(&self, spot: crate::ui::chat::Spot) -> Option<Click> {
        let row = self.wrapped.get(spot.line)?;
        if self.pending_approval.is_some() {
            let mut at = 0usize;
            for span in &row.spans {
                let wide = span.content.chars().count();
                let letter = span.content.trim();
                let badge = span.content.len() == 3
                    && span.content.starts_with(' ')
                    && span.content.ends_with(' ')
                    && matches!(letter, "y" | "a" | "n" | "e" | "x")
                    && span.style.bg == Some(crate::ui::theme::theme().warn);
                if badge && spot.column >= at && spot.column < at + wide {
                    return letter.chars().next().map(Click::Key);
                }
                at += wide;
            }
        }
        let entry = self.wrapped.block_at(spot.line)?;
        self.entries
            .get(entry)
            .filter(|e| e.can_unfold())
            .map(|_| Click::Unfold(entry))
    }

    /// Where the running step of the call `id` sits, if one is.
    fn step_of_call(&self, id: &str) -> Option<usize> {
        self.entries
            .iter()
            .rposition(|e| matches!(e, Entry::Step(s) if s.id == id && s.outcome.is_none()))
    }

    /// Settle the step a result answers, found by the id the call and the result share.
    ///
    /// The result holds the whole output, so what the tool printed while it ran is dropped.
    /// A result whose call is not on screen gets a step of its own.
    pub(crate) fn settle_step(&mut self, id: &str, name: String, outcome: Outcome) {
        self.stops.remove(id);
        let at = self
            .entries
            .iter()
            .rposition(|e| matches!(e, Entry::Step(s) if s.id == id && s.outcome.is_none()));
        match at {
            Some(at) => {
                if let Entry::Step(step) = &mut self.entries[at] {
                    step.live.clear();
                    step.outcome = Some(outcome);
                }
            }
            None => self.entries.push(Entry::Step(Step {
                id: id.to_string(),
                name,
                outcome: Some(outcome),
                ..Default::default()
            })),
        }
    }

    /// Add a piece of the answer to the one entry it is being written into.
    pub(crate) fn write_answer(&mut self, chunk: String) {
        match self.answer_being_written() {
            Some(at) => match &mut self.entries[at] {
                Entry::AssistantStreaming(s) => s.push_str(&chunk),
                _ => unreachable!("answer_being_written names an answer"),
            },
            None => self.entries.push(Entry::AssistantStreaming(chunk)),
        }
    }

    /// Settle the answer in the place it was written, rather than at the end.
    ///
    /// The finished text replaces the streamed text in its entry.
    pub(crate) fn finish_answer(&mut self, text: String) {
        match (self.answer_being_written(), text.trim().is_empty()) {
            (Some(at), false) => self.entries[at] = Entry::Assistant(text),
            (Some(at), true) => {
                self.entries.remove(at);
            }
            (None, false) => self.entries.push(Entry::Assistant(text)),
            (None, true) => {}
        }
    }

    /// Apply what a session run by the call `call` did to that call's step: a step it
    /// asked for, a step settled, its answer.
    pub(crate) fn apply_sub(&mut self, call: Option<&str>, sub: crate::tools::registry::SubEvent) {
        use crate::tools::registry::SubEvent;
        let Some(at) = call.and_then(|id| self.step_of_call(id)) else {
            return;
        };
        let Entry::Step(step) = &mut self.entries[at] else {
            return;
        };
        match sub {
            SubEvent::Called { id, name, args } => {
                step.children.push(Step::called(id, name, &args));
            }
            SubEvent::Settled {
                id,
                is_error,
                body,
                took,
                origin,
            } => {
                if let Some(child) = step
                    .children
                    .iter_mut()
                    .rev()
                    .find(|c| c.id == id && c.outcome.is_none())
                {
                    child.outcome = Some(Outcome {
                        is_error,
                        body,
                        took,
                        origin,
                    });
                }
            }
            SubEvent::Answer(text) => step.said = text,
        }
    }

    /// Show the end of what a tool has printed under the step that is running it.
    ///
    /// A line from a tool with no step running is dropped.
    /// The result carries it.
    pub(crate) fn show_tail(&mut self, call: Option<&str>, tool: &str, tail: String) {
        // By the call when it says which, so calls to one tool side by side keep their output
        // apart.
        // A call that is not a step here, one made inside a delegated session, shows nothing.
        let at = match call {
            Some(id) => self.step_of_call(id),
            None => self.step_running(tool),
        };
        let Some(at) = at else {
            return;
        };
        let Entry::Step(Step { live, .. }) = &mut self.entries[at] else {
            unreachable!("step_running names a step")
        };
        *live = tail;
    }

    /// Put the screen back to how it opened, keeping what a session is rather than what
    /// was said in it.
    pub(crate) fn start_again(&mut self) {
        self.entries.clear();
        self.tasks.clear();
        self.unfolded.clear();
        self.at_entry = None;
        self.finding = None;
        self.selecting = None;
        self.scroll = 0;
        self.step = 0;
        self.window = Default::default();
        self.rates.clear();
    }

    /// Note the power the machine draws, keeping the latest few readings.
    pub(crate) fn note_power(&mut self, power: crate::backend::devices::Power) {
        self.power = Some(power);
        self.watts.push_back(power.watts);
        while self.watts.len() > crate::ui::rail::RATES_KEPT {
            self.watts.pop_front();
        }
    }

    /// Note how fast a step came back, keeping the latest few for the sparkline.
    pub(crate) fn note_rate(&mut self, tokens_a_second: f64) {
        self.rate = Some(tokens_a_second);
        self.rates.push_back(tokens_a_second);
        while self.rates.len() > crate::ui::rail::RATES_KEPT {
            self.rates.pop_front();
        }
    }

    /// Put a resumed session on screen: what was said, where it was said.
    pub(crate) fn show_resumed(&mut self, resumed: crate::agent::agent_loop::Resumed) {
        self.start_again();
        self.planning = resumed.planning;
        let said = resumed.messages.len();
        self.entries.extend(what_was_said(&resumed.messages));
        if let Some(model) = resumed.model {
            self.entries
                .push(Entry::Notice(format!("back on {model}, as it was left")));
        }
        self.entries.push(Entry::Notice(format!(
            "session {}: {said} message(s) back",
            resumed.id
        )));
    }

    /// Accumulate a piece of the model's scratchpad into one entry.
    pub(crate) fn push_reasoning(&mut self, chunk: &str) {
        match self.entries.last_mut() {
            Some(Entry::Reasoning(s)) => s.push_str(chunk),
            _ => self.entries.push(Entry::Reasoning(chunk.to_string())),
        }
    }

    pub(crate) fn remember(&mut self, text: &str) {
        // Repeating the last line adds nothing to walk through.
        if self.history.last().map(String::as_str) != Some(text) {
            self.history.push(text.to_string());
        }
        self.history_at = None;
    }

    /// The previous thing sent, or `None` at the beginning of the history.
    pub(crate) fn older(&mut self) -> Option<String> {
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
    pub(crate) fn newer(&mut self) -> Option<String> {
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

/// Whether an entry is one somebody would go looking for.
///
/// A question, an answer or a step.
/// The remarks between them are never sought out.
pub(crate) fn worth_pointing_at(e: &Entry) -> bool {
    matches!(
        e,
        Entry::User(_) | Entry::Assistant(_) | Entry::AssistantStreaming(_) | Entry::Step(_)
    )
}

/// How long an answer may be before it goes on a page of its own rather than into the
/// conversation.
///
/// A longer answer is a document, and a document in the thread is in the way of everything
/// after it.
pub(crate) const SHEET_LINES: usize = 3;

#[cfg(test)]
mod tests {
    use super::*;

    /// An answer settles where it was written, however much arrived under it.
    ///
    /// A notice between the last piece of an answer and the finished text must not show the answer
    /// twice.
    #[test]
    fn an_answer_interrupted_by_a_notice_is_still_shown_once() {
        let mut state = UiState::default();
        state.write_answer("the first half ".into());
        state.write_answer("and the second".into());
        state
            .entries
            .push(Entry::Notice("context budget now".into()));
        state.finish_answer("the first half and the second".into());

        assert_eq!(state.entries.len(), 2);
        assert!(
            matches!(&state.entries[0], Entry::Assistant(s) if s == "the first half and the second")
        );
        assert!(matches!(&state.entries[1], Entry::Notice(_)));
    }

    /// A second answer in the same turn is its own entry, not an addition to the first.
    #[test]
    fn each_answer_of_a_turn_is_its_own() {
        let mut state = UiState::default();
        state.write_answer("first".into());
        state.finish_answer("first".into());
        state.write_answer("second".into());
        state.finish_answer("second".into());
        assert_eq!(state.entries.len(), 2);
        assert!(matches!(&state.entries[1], Entry::Assistant(s) if s == "second"));
    }

    /// An answer that turns out to be empty leaves nothing behind, wherever it sat.
    #[test]
    fn an_answer_that_came_to_nothing_is_taken_away() {
        let mut state = UiState::default();
        state.write_answer("  ".into());
        state
            .entries
            .push(Entry::Notice("something happened".into()));
        state.finish_answer("  ".into());
        assert_eq!(state.entries.len(), 1);
        assert!(matches!(&state.entries[0], Entry::Notice(_)));
    }

    /// A tool's lines go to its step even when a notice arrived in between, and its
    /// result takes their place.
    #[test]
    fn the_running_output_of_a_tool_is_found_under_what_interrupted_it() {
        let mut state = UiState::default();
        state
            .entries
            .push(Entry::Step(Step::called("7".into(), "shell".into(), "{}")));
        state.show_tail(None, "shell", "compiling".into());
        state
            .entries
            .push(Entry::Notice("context budget now".into()));
        state.show_tail(None, "shell", "compiling\nlinking".into());
        assert_eq!(state.entries.len(), 2);
        assert!(matches!(
            &state.entries[0],
            Entry::Step(Step { live, .. }) if live == "compiling\nlinking"
        ));
        assert_eq!(state.step_running("shell"), Some(0));
        assert_eq!(state.step_running("read_file"), None);
        state.settle_step(
            "7",
            "shell".into(),
            Outcome {
                is_error: false,
                body: "built".into(),
                took: Duration::ZERO,
                origin: crate::agent::agent_loop::Origin::Ran,
            },
        );
        assert!(matches!(
            &state.entries[0],
            Entry::Step(Step { live, outcome: Some(_), .. }) if live.is_empty()
        ));
        assert_eq!(state.step_running("shell"), None);
    }

    /// Lines from 2 calls of one tool running side by side go to their own steps.
    #[test]
    fn calls_side_by_side_keep_their_lines_apart() {
        let mut state = UiState::default();
        state.entries.push(Entry::Step(Step::called(
            "a".into(),
            "delegate".into(),
            "{}",
        )));
        state.entries.push(Entry::Step(Step::called(
            "b".into(),
            "delegate".into(),
            "{}",
        )));
        state.show_tail(Some("a"), "delegate", "read_file  src/x.rs".into());
        state.show_tail(Some("b"), "delegate", "grep  timeout".into());
        state.show_tail(Some("elsewhere"), "bash", "dropped".into());
        let live = |at: usize| match &state.entries[at] {
            Entry::Step(s) => s.live.clone(),
            _ => String::new(),
        };
        assert_eq!(live(0), "read_file  src/x.rs");
        assert_eq!(live(1), "grep  timeout");
    }

    /// The list of running things points at a step when one is chosen, and stops work
    /// aside when it is chosen, looking into the project staying stopped.
    #[test]
    fn the_list_of_running_things_points_and_stops() {
        let mut state = UiState::default();
        state
            .entries
            .push(Entry::Step(Step::called("7".into(), "bash".into(), "{}")));
        let stop = tokio_util::sync::CancellationToken::new();
        state.stops.insert("7".into(), stop.clone());
        let looking = tokio_util::sync::CancellationToken::new();
        state.aside.push(Aside {
            kind: AsideKind::Discover,
            what: "looking into: how is it tested".into(),
            since: Instant::now(),
            stop: looking.clone(),
        });
        let jobs = state.jobs();
        assert_eq!(jobs.len(), 2);
        assert!(jobs.iter().all(|j| j.stoppable));
        assert!(state.open_jobs(None));
        state.take_job(1);
        assert_eq!(state.at_entry, Some(0));
        assert!(state.pointed_stop().is_some());
        assert!(state.open_jobs(None));
        state.take_job(2);
        assert!(looking.is_cancelled());
        assert!(state.discovery_stopped);
        assert!(state.aside.is_empty());
        assert!(!stop.is_cancelled());
    }

    /// An idle screen sets no timer.
    /// Work, a held drag and a fresh remark each set one.
    #[test]
    fn nothing_wakes_an_idle_screen() {
        let mut s = UiState::default();
        assert_eq!(s.next_wake(), None);
        s.busy = true;
        assert_eq!(s.next_wake(), Some(TICK_EVERY));
        s.drag_edge = Some(1);
        assert_eq!(s.next_wake(), Some(DRAG_EVERY));
        let mut s = UiState {
            said: Some(("taken".into(), Instant::now())),
            ..Default::default()
        };
        assert!(s.next_wake().is_some_and(|w| w <= SAID_FOR));
        s.said = Some(("taken".into(), Instant::now() - SAID_FOR * 2));
        assert_eq!(s.next_wake(), None);
    }

    /// A long run keeps its tail, and no more than that.
    ///
    /// The end of a run is held, since it is what says the run is going wrong.
    #[test]
    fn a_tool_that_prints_for_minutes_keeps_its_tail() {
        let mut state = UiState::default();
        state
            .entries
            .push(Entry::Step(Step::called("1".into(), "shell".into(), "{}")));
        let tail = crate::tools::registry::Tail::default();
        let kept = crate::tools::registry::TAIL_LINES;
        let told = (0..kept + 50)
            .filter(|i| tail.push(format!("line {i}")))
            .count();
        // Told once, however many lines came before the screen read them.
        assert_eq!(told, 1);
        state.show_tail(None, "shell", tail.read());
        let Entry::Step(Step { live: body, .. }) = &state.entries[0] else {
            panic!("one block of output")
        };
        assert_eq!(body.lines().count(), kept);
        assert!(body.ends_with(&format!("line {}", kept + 49)));
        assert!(
            tail.push("more".into()),
            "a line after a read tells the screen again"
        );
        // Folded, only the end of it is on screen.
        // Unfolded, all of it is.
        assert!(state.entries[0].render(false, false).len() < kept);
        assert!(state.entries[0].render(true, false).len() > kept);
    }

    /// A row on screen is a line of the conversation, and which line depends on where the
    /// pane has been scrolled to.
    ///
    /// A selection goes wrong without looking wrong when the highlight and the text taken disagree.
    /// The mapping is pinned at the top of the pane, at its bottom, scrolled back, and outside it.
    #[test]
    fn a_row_on_screen_is_the_line_the_pane_shows_there() {
        let mut state = UiState {
            wrapped: (0..100)
                .map(|i| Line::from(format!("line {i} with some text")))
                .collect::<Vec<_>>()
                .into(),
            pane: ratatui::layout::Rect::new(0, 0, 40, 10),
            ..Default::default()
        };
        state.content_lines = state.wrapped.len();

        // At the foot of the conversation the last 10 lines are shown, so the top row is
        // line 90 and the bottom row is line 99.
        assert_eq!(state.spot_at(0, 0).unwrap().line, 90);
        assert_eq!(state.spot_at(0, 9).unwrap().line, 99);
        // Scrolled back by 5, everything moves with it.
        state.scroll = 5;
        assert_eq!(state.spot_at(0, 0).unwrap().line, 85);
        assert_eq!(state.spot_at(0, 9).unwrap().line, 94);
        // The column is where the pointer is, never past the end of what the line holds.
        assert_eq!(state.spot_at(6, 0).unwrap().column, 6);
        assert_eq!(
            state.spot_at(39, 0).unwrap().column,
            crate::ui::chat::line_width(state.wrapped.get(85).unwrap())
        );
        // Below the pane is not the conversation.
        assert!(state.spot_at(0, 10).is_none());
        // A drag that leaves the pane still reaches the row it left by.
        assert_eq!(state.spot_at_clamped(0, 30).line, 94);
        assert_eq!(state.spot_at_clamped(0, 0).line, 85);
    }

    /// An empty drag is not a selection.
    /// Pressing without moving must not replace the clipboard with nothing.
    #[test]
    fn a_press_that_selected_nothing_takes_nothing() {
        let mut state = UiState {
            wrapped: vec![Line::from("some text")].into(),
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

    /// Quit is consulted before the approval branch, so these are recognised as quit.
    /// The modal would otherwise read them as "not a decision".
    fn scrollable(content: usize, view: usize) -> UiState {
        UiState {
            content_lines: content,
            view_height: view,
            ..Default::default()
        }
    }

    /// Scrolling past the top stops at the oldest line.
    /// The window must not slide off the end of the content.
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

    /// The up arrow walks back through what was sent.
    /// The down arrow returns to an empty input rather than sticking on the last entry.
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
}
