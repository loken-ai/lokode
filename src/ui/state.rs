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
/// What a person said and what the model answered. The traffic between them - a tool
/// called, what it printed - is in the record and reaches the model, but a screen of it
/// on the way back into a session is a screen of what was already dealt with.
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
/// A sentence belongs in the thread, where it reads as what just happened. A page of it
/// does not: it is read once, it is not part of the conversation, and left there it sits
/// between two questions for the rest of the session.
pub(crate) fn show(state: &mut UiState, verb: &str, reply: String) {
    match reply.lines().count() > SHEET_LINES {
        true => state.sheet = Some(crate::ui::sheet::Sheet::new(format!("/{verb}"), &reply)),
        false => state.entries.push(Entry::Notice(reply)),
    }
}

/// How long the status line keeps saying what was just done.
pub(crate) const SAID_FOR: Duration = Duration::from_secs(4);

/// How much of a running tool's output is kept for the rest to be unfolded onto.
///
/// A build that fails a hundred lines before it stops used to have those hundred lines
/// dropped as they arrived, so the only way to see them was to wait for the run to end.
pub(crate) const KEPT_OUTPUT_LINES: usize = 400;

/// How often the spinner moves and the phase clock counts, while there is work to show.
const TICK_EVERY: Duration = Duration::from_millis(120);

/// How often a drag held past an edge scrolls the pane one more line.
const DRAG_EVERY: Duration = Duration::from_millis(80);

#[derive(Default)]
pub(crate) struct UiState {
    pub(crate) entries: Thread,
    /// What each entry was last drawn as, so a frame draws only the entries that changed.
    pub(crate) drawings: Drawings,
    pub(crate) scroll: u16,
    pub(crate) pending_approval: Option<ApprovalPrompt>,
    /// Where a drag with the mouse began and where it is now, in lines of the wrapped
    /// pane. Both ends are kept because the person may drag in either direction.
    pub(crate) selecting: Option<(crate::ui::chat::Spot, crate::ui::chat::Spot)>,
    /// When quitting was last asked for and not confirmed. A session holds work nobody
    /// else has - what was said, what was changed, what can still be put back - so leaving
    /// it is asked for twice.
    pub(crate) quit_asked: Option<Instant>,
    /// Whether writes go ahead unasked, for this stretch of work.
    pub(crate) accepting_writes: bool,
    /// What is being looked for in the conversation, and which match is shown.
    ///
    /// A long session holds what a tool answered an hour ago, and the only way back to it
    /// was the wheel. The match is pointed at rather than extracted, so what is found is
    /// read in the place it happened.
    pub(crate) finding: Option<(String, usize)>,
    /// When Esc was last pressed with nothing for it to do. Pressed twice it takes the
    /// last question back, which is the shortcut for the commonest correction there is.
    pub(crate) esc_at: Option<Instant>,
    /// Which edge a drag is being held past, if any: the pane scrolls that way for as
    /// long as it is, because a terminal reports a moving pointer and not a still one.
    pub(crate) drag_edge: Option<i8>,
    /// What was just done and when, for the status line to say while it is still news.
    pub(crate) said: Option<(String, Instant)>,
    /// A list a command opened, waiting to be chosen from.
    pub(crate) choosing: Option<crate::ui::chooser::Chooser>,
    /// A call being refused, while what to do instead is being typed.
    pub(crate) refusing: Option<crate::sandbox::approval::ApprovalPrompt>,
    /// What was typed during a turn to be asked once it is over.
    pub(crate) queued: Vec<String>,
    /// How many entries the search matched, for saying which one is being shown.
    pub(crate) found: usize,
    /// How fast the last step came back, in tokens a second.
    pub(crate) rate: Option<f64>,
    /// A line cleared off the input, kept for the up arrow to offer back.
    pub(crate) draft: Option<String>,
    /// What is being done for this session outside any turn, and since when.
    pub(crate) background: Option<(String, Instant)>,
    /// Whether the mouse belongs to this program or to the terminal.
    ///
    /// Taking it buys the wheel and a selection that can scroll past the edge of the
    /// pane. It costs the terminal's own selection, and with it the terminal's own copy
    /// key - which is the one everybody's hands already know.
    pub(crate) mouse_ours: bool,
    /// Whether the two ways of copying have been said this session.
    pub(crate) said_how_to_copy: bool,
    /// What the backend serves, for finishing the name of one that is being typed. Asked
    /// for once and after a switch, since a backend's catalogue does not move under a
    /// session.
    pub(crate) models: Vec<String>,
    /// Whether the button is still down: a drag that has been let go stays on screen,
    /// so it can be seen and so a second look does not have to be a second drag.
    pub(crate) dragging: bool,
    /// The pane as it was last drawn, wrapped to its width. What the mouse points at is a
    /// row of this, and nothing else can say which line a row holds.
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
    /// Whether the agent is planning. Held here because the mode lives with the agent,
    /// and the interface only learns of a change by having asked for it.
    pub(crate) planning: bool,
    /// How full the window is and with what, as the agent last measured it.
    pub(crate) window: crate::agent::session::Occupancy,
    /// The speeds of the latest steps, oldest first, for the rail's sparkline.
    pub(crate) rates: std::collections::VecDeque<f64>,
    /// The files this session changed, as the change history last said.
    pub(crate) changes: Vec<crate::ui::rail::FileChange>,
    /// The turns that can be put back, oldest first.
    pub(crate) turns: Vec<crate::checkpoint::Turn>,
    /// Whether the rail is shown: `None` follows the width of the screen, and a key sets
    /// it either way.
    pub(crate) rail: Option<bool>,
    /// How far the server has read the prompt, in tokens, while it says.
    pub(crate) prefill: Option<(u64, u64)>,
    /// What the backend measured, as of the latest step.
    pub(crate) engine: crate::agent::agent_loop::Engine,
    /// Whether the terminal window has the focus, as it last said.
    pub(crate) focused: bool,
    /// The window title as it was last set.
    pub(crate) title: String,
    /// The progress the tab was last told, so it is written only when it changes.
    pub(crate) shown_progress: Option<crate::ui::term::Progress>,
    /// Whether the rail was on screen in the last frame, for the key that flips it.
    pub(crate) rail_now: bool,
    /// How tool calls travel, as the agent has it: settled by asking the backend, so the
    /// interface can only know it by asking.
    pub(crate) protocol: crate::agent::protocol::ToolProtocol,
    /// When the approval now on screen was asked for. `None` when nothing is waiting.
    pub(crate) waiting_since: Option<Instant>,
    /// The list open over the input, when the token being typed has one.
    pub(crate) picker: Option<crate::ui::picker::Picker>,
    /// A command's answer, on a page over the conversation until it is closed.
    pub(crate) sheet: Option<crate::ui::sheet::Sheet>,
    /// The entry the keys are pointing at, when the user has moved off the newest.
    ///
    /// `None` means the bottom of the conversation, where a session spends its time; a
    /// mark is only worth drawing once somebody has gone looking for something.
    pub(crate) at_entry: Option<usize>,
    /// Entries shown whole rather than counted.
    pub(crate) unfolded: std::collections::HashSet<usize>,
    /// The task list as it was last drawn.
    pub(crate) tasks: Vec<crate::tools::plan::Task>,
    /// The model answering now. It changes under the interface - by a command, by the
    /// mode, by a check that failed - so it is asked for rather than assumed.
    pub(crate) model: String,
    /// Abandons the turn in flight. `None` between turns.
    pub(crate) turn_cancel: Option<CancellationToken>,
    /// What the chat pane last drew, so scrolling can be bounded by what exists.
    ///
    /// Known only after a draw, which is why it is remembered rather than computed: the
    /// height comes from the terminal and the wrapping from its width.
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
    /// A turn or background work moves the spinner and the clocks beside it; a drag held
    /// past an edge keeps scrolling, since the terminal reports a pointer that moves and
    /// not one held still; a remark on the status line goes when its time is up.
    pub(crate) fn next_wake(&self) -> Option<Duration> {
        let mut wake = None::<Duration>;
        let mut sooner = |after: Duration| {
            wake = Some(wake.map_or(after, |w| w.min(after)));
        };
        if self.busy || self.background.is_some() {
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
    /// Put at the top rather than centred: what a person wants to read starts there and
    /// runs downward, and centring hides the half of it they were looking for.
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
    ///
    /// From the most recent because that is where a person is: what was said a moment ago
    /// is what they are usually trying to find again, and walking forward from the start
    /// of a long session would make them press the key a hundred times to get there.
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
    /// A drag that leaves the pane is still a drag: letting go outside it must take what
    /// was crossed rather than nothing, which is what a strict answer here would give.
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
    /// A terminal reports a pointer that moves. Held still outside the pane it says
    /// nothing more, so without this a selection stopped growing the moment the person
    /// stopped moving - which is exactly when they were waiting for it to catch up.
    pub(crate) fn follow_the_drag(&mut self) {
        let Some(edge) = self.drag_edge else { return };
        let Some((from, _)) = self.selecting else {
            return;
        };
        match edge {
            -1 => self.scroll_back(1),
            _ => self.scroll_forward(1),
        }
        // The far end goes to the line now at that edge, so what is highlighted is what
        // has been dragged over.
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
    /// Only the entries worth stopping on: pointing at every blank remark between two
    /// steps makes the key useless for finding the step.
    pub(crate) fn point_back(&mut self) {
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
    /// Searched for rather than assumed to be last. What arrives while a model writes is
    /// not only its own words: a notice, a line from a tool still running, a budget that
    /// moved. An answer with one of those under it is still the answer being written.
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

    /// Settle the step a result answers, found by the id the call and the result share.
    ///
    /// What it printed while it ran goes: the result holds the whole of it. A result whose
    /// call is not on screen gets a step of its own.
    pub(crate) fn settle_step(&mut self, id: &str, name: String, outcome: Outcome) {
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
                subject: String::new(),
                live: String::new(),
                outcome: Some(outcome),
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
    /// The finished text used to be appended whenever anything had arrived under the
    /// stream, leaving the half that had streamed above it and the whole of it again
    /// below: the same answer twice, on every turn the session had something to say.
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

    /// Add a line a tool just printed to the step that is running it.
    ///
    /// A line from a tool with no step running is dropped: the result carries it anyway.
    pub(crate) fn print_line(&mut self, tool: &str, line: &str) {
        let Some(at) = self.step_running(tool) else {
            return;
        };
        let Entry::Step(Step { live: body, .. }) = &mut self.entries[at] else {
            unreachable!("step_running names a step")
        };
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(line);
        // Only the tail is kept, and only the end of that is shown: the whole of it
        // reaches the model in the result, and what a person wants while it runs is where
        // it is now, with the rest a keypress away.
        let lines: Vec<&str> = body.lines().collect();
        if lines.len() > KEPT_OUTPUT_LINES {
            *body = lines[lines.len() - KEPT_OUTPUT_LINES..].join("\n");
        }
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

    /// Note how fast a step came back, keeping the latest few for the sparkline.
    pub(crate) fn note_rate(&mut self, tokens_a_second: f64) {
        self.rate = Some(tokens_a_second);
        self.rates.push_back(tokens_a_second);
        while self.rates.len() > crate::ui::rail::RATES_KEPT {
            self.rates.pop_front();
        }
    }

    /// Put a resumed session on screen: what was said, where it was said.
    ///
    /// The count alone used to be reported as "back in view", which they were not: the
    /// model could see a conversation the person could not, and the first question of the
    /// new turn read as the first question of the session.
    pub(crate) fn show_resumed(&mut self, resumed: crate::agent::agent_loop::Resumed) {
        self.start_again();
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
/// A question, an answer, and a step: the things a session is made of. Not the remarks
/// between them, which are read as they go by and never sought out.
pub(crate) fn worth_pointing_at(e: &Entry) -> bool {
    matches!(
        e,
        Entry::User(_) | Entry::Assistant(_) | Entry::AssistantStreaming(_) | Entry::Step(_)
    )
}

/// How long an answer may be before it goes on a page of its own rather than into the
/// conversation.
///
/// Three lines is a sentence and its detail, which reads as something that just happened.
/// More than that is a document, and a document in the thread is in the way of everything
/// after it.
pub(crate) const SHEET_LINES: usize = 3;

#[cfg(test)]
mod tests {
    use super::*;

    /// An answer settles where it was written, however much arrived under it.
    ///
    /// The reported defect: the session said "context budget now 440055 chars" between
    /// the last piece of an answer and the finished text, and the whole answer was shown
    /// twice - once as it had streamed, once again below the notice.
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
        state.print_line("shell", "compiling");
        state
            .entries
            .push(Entry::Notice("context budget now".into()));
        state.print_line("shell", "linking");
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

    /// An idle screen sets no timer; work, a held drag and a fresh remark each set one.
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
    /// Held rather than shown: the end of it is what says a run is going wrong, and the
    /// lines before it used to be dropped as they arrived, so the only way to see them
    /// was to wait for the run to finish.
    #[test]
    fn a_tool_that_prints_for_minutes_keeps_its_tail() {
        let mut state = UiState::default();
        state
            .entries
            .push(Entry::Step(Step::called("1".into(), "shell".into(), "{}")));
        for i in 0..KEPT_OUTPUT_LINES + 50 {
            state.print_line("shell", &format!("line {i}"));
        }
        let Entry::Step(Step { live: body, .. }) = &state.entries[0] else {
            panic!("one block of output")
        };
        assert_eq!(body.lines().count(), KEPT_OUTPUT_LINES);
        assert!(body.ends_with(&format!("line {}", KEPT_OUTPUT_LINES + 49)));
        // Folded, only the end of it is on screen; unfolded, all of it is.
        assert!(state.entries[0].render(false, false).len() < KEPT_OUTPUT_LINES);
        assert!(state.entries[0].render(true, false).len() > KEPT_OUTPUT_LINES);
    }

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
                .collect::<Vec<_>>()
                .into(),
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
            crate::ui::chat::line_width(state.wrapped.get(85).unwrap())
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
}
