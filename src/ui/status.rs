use std::time::Duration;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::ui::chat::how_long;
use crate::ui::theme::theme;

#[derive(Debug, Clone)]
pub struct Status {
    /// Shown only while planning: acting is the ordinary state and does not need saying.
    pub planning: bool,
    /// Whether the user is looking through the conversation rather than typing into it.
    pub pointing: bool,
    /// How much of the history budget the conversation occupies, as a percentage.
    ///
    /// Against the budget rather than the model's whole window: the budget is what
    /// compaction defends, so a hundred here is where the oldest steps start being dropped
    /// rather than where the model would refuse the request. Coloured as it approaches,
    /// because the first elision is the moment a session starts forgetting what it was
    /// told.
    pub context_percent: u8,
    /// Whether the entry pointed at is a receipt whose turn changed files.
    pub on_receipt: bool,
    /// What is open over the rest, which owns the keys while it is.
    pub open: Option<Open>,
    /// The files the line being typed names with `@`, and the characters they hold.
    pub attaching: Option<(usize, u64)>,
    /// The plan's tasks done and in all, said here when the rail is not on screen.
    pub plan: Option<(usize, usize)>,
    /// Whether the screen is too narrow for the rail to be there unasked, which puts the
    /// key that opens it on the line of keys.
    pub rail_offered: bool,
    pub step: usize,
    pub max_steps: usize,
    pub busy: bool,
    /// Whether there is something in the input, which changes what leaving means.
    pub typing: bool,
    /// Whether something is selected in the pane, which puts taking it on the line.
    pub selected: bool,
    /// Whether this program holds the mouse. Never invisible: a wheel that has stopped
    /// scrolling the conversation is otherwise a program that has broken.
    pub mouse_ours: bool,
    /// How many messages are waiting for this turn to end.
    pub queued: usize,
    /// What is being searched for, and which match is being shown, while a search is on.
    pub finding: Option<String>,
    /// How fast the last step came back, in tokens a second.
    pub rate: Option<f64>,
    /// What is being done outside any turn, and for how long. Work that belongs to no
    /// turn still takes time, and a screen that says nothing about it looks broken.
    pub background: Option<(String, Duration)>,
    /// Whether writes are going through unasked. A third setting beside planning and
    /// acting, and the one a person most needs reminding they are in.
    pub accepting_writes: bool,
    /// What the turn is doing, when it is doing something that prints nothing.
    pub phase: Option<String>,
    /// What just happened, for a few seconds, when it happened somewhere the person is
    /// not looking. A conversation scrolled back does not show what was appended to its
    /// foot, so an answer to a key that was just pressed goes here instead.
    pub said: Option<String>,
    /// How long it has been in that phase.
    pub in_phase: Duration,
    /// Advances while the turn is busy, so a screen with nothing else moving still says
    /// the program is alive rather than wedged.
    pub tick: usize,
}

/// The frames of the spinner, one per tick.
const SPINNER: [&str; 4] = ["◐", "◓", "◑", "◒"];

/// What can be open over the conversation and own the keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Open {
    /// A command's page.
    Page,
    /// A list a command opened.
    Choice,
    /// The completion list over the input.
    Completion,
}

impl Status {
    /// The line under the input: what is in force on the left, what is happening on the
    /// right.
    ///
    /// Everything here answers a question somebody actually asks mid-turn - which model,
    /// how full, what is it doing, how long has it been doing it. What was in it and is
    /// not: the session id, which nobody types; the absolute path of the project, whose
    /// last component is the part that identifies it; and the protocol, which is settled
    /// once and said when it changes.
    pub fn to_line(&self) -> Line<'static> {
        let mut spans = Vec::new();
        spans.push(Span::styled(
            match self.busy {
                true => format!(" {} ", SPINNER[self.tick % SPINNER.len()]),
                false => "   ".to_string(),
            },
            Style::default().fg(if self.busy {
                theme().accent
            } else {
                theme().faint()
            }),
        ));
        let (badge, colour) = match (self.planning, self.accepting_writes) {
            (true, _) => (" PLAN ", theme().accent),
            // Loudest of the three, because it is the one where things happen without
            // being shown first.
            (false, true) => (" WRITE", theme().warn),
            (false, false) => (" ACT  ", theme().good),
        };
        spans.push(Span::styled(
            badge,
            Style::default()
                .fg(theme().ink)
                .bg(colour)
                .add_modifier(Modifier::BOLD),
        ));
        // What the turn is doing comes first, beside the mode: it is what the eye looks for
        // when the screen has stopped changing. The model and the project are on the band
        // at the top.
        if let Some(phase) = self.phase.as_deref().filter(|_| self.busy) {
            spans.push(Span::styled(
                format!("  {phase} {}", how_long(self.in_phase)),
                Style::default()
                    .fg(theme().warn)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        // Named, because a bare percentage on a status bar is a number nobody can act
        // on: this one says how close the conversation is to the point where its oldest
        // steps start being dropped to make room.
        spans.push(Span::styled(
            format!("  ctx {}%", self.context_percent),
            Style::default().fg(match self.context_percent {
                0..=74 => theme().faint(),
                75..=99 => theme().warn,
                _ => theme().bad,
            }),
        ));
        // What the files named on the line will cost before it is sent: on a local model
        // a long attachment is minutes of prompt reading.
        if let Some((files, chars)) = self.attaching {
            spans.push(Span::styled(
                format!(
                    "  attaching {files} file(s) ≈{} tokens",
                    crate::ui::chat::tokens(chars / crate::agent::session::CHARS_PER_TOKEN as u64)
                ),
                Style::default().fg(theme().accent),
            ));
        }
        if let Some((done, all)) = self.plan {
            spans.push(Span::styled(format!("  plan {done}/{all}"), theme().dim));
        }
        if self.busy {
            spans.push(Span::styled(
                format!("  step {}/{}", self.step, self.max_steps),
                theme().dim,
            ));
        }
        if let Some((what, since)) = self.background.as_ref() {
            spans.push(Span::styled(
                format!("  {what} {}", how_long(*since)),
                Style::default().fg(theme().accent),
            ));
        }
        // A turn that takes a minute with nothing on screen looks the same as a turn that
        // has stalled. This is the number that tells them apart, and it is only shown
        // while there is a turn for it to be about.
        if let Some(rate) = self.rate.filter(|_| self.busy) {
            spans.push(Span::styled(format!("  {rate:.0} tok/s"), theme().dim));
        }
        // The phase is what a user is looking for when the screen has stopped changing, so
        // it goes last, where the eye lands, with the time it has been there beside it.
        // A search owns the keyboard while it is open, so the line says so and says what
        // the keys mean there: without it the only sign a search was on was that typing
        // did not reach the input.
        if let Some(finding) = self.finding.as_deref() {
            spans.push(Span::styled(
                format!("  {finding}"),
                Style::default()
                    .fg(theme().ink)
                    .bg(theme().warn)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        if self.queued > 0 {
            spans.push(Span::styled(
                format!("  {} queued", self.queued),
                Style::default().fg(theme().accent),
            ));
        }
        if let Some(said) = self.said.as_deref() {
            spans.push(Span::styled(
                format!("  {said}"),
                Style::default().fg(theme().accent),
            ));
        }
        Line::from(spans)
    }

    /// The keys, on their own line, because a shortcut nobody can see is a shortcut nobody
    /// uses. Only the ones that change what happens next.
    ///
    /// `pointing` is true once the user has gone looking through the conversation, where
    /// a different set of keys is in force and the ones for typing are not.
    /// The keys in force, in the width there is for them.
    ///
    /// Dropped from the least useful end rather than cut by the terminal: a line that runs
    /// off the edge loses whichever key happens to be last, silently, and this project's
    /// own reason for having the line at all is that a shortcut nobody can see is a
    /// shortcut nobody uses. What is left ends in a mark, so a reader knows the list goes
    /// on and that `/help` has the rest.
    pub fn keys_within(&self, width: usize) -> Line<'static> {
        let full = self.keys();
        let shown: usize = full.spans.iter().map(|s| s.content.chars().count()).sum();
        if width == 0 || shown <= width {
            return full;
        }
        // Pairs, because a key without what it does is worse than no key.
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut used = 0usize;
        for pair in full.spans.chunks(2) {
            let cost: usize = pair.iter().map(|s| s.content.chars().count()).sum();
            // Two columns kept back for the mark that says there is more.
            if used + cost + 2 > width {
                break;
            }
            used += cost;
            spans.extend(pair.iter().cloned());
        }
        spans.push(Span::styled(" …", theme().dim));
        Line::from(spans)
    }

    pub fn keys(&self) -> Line<'static> {
        let key = |k: &str, what: &str| {
            vec![
                Span::styled(format!("  {k}"), theme().key),
                Span::styled(format!(" {what}"), theme().dim),
            ]
        };
        let mut spans = Vec::new();
        // What is open over the rest owns the keys, so the line says its keys and no others.
        match self.open {
            Some(Open::Page) => {
                spans.extend(key("esc", "close"));
                spans.extend(key("↑↓", "scroll"));
                spans.extend(key("pgup pgdn", "page"));
                spans.extend(key("home end", "ends"));
                return Line::from(spans);
            }
            Some(Open::Choice) => {
                spans.extend(key("enter", "choose"));
                spans.extend(key("↑↓", "move"));
                spans.extend(key("type", "narrow"));
                spans.extend(key("esc", "cancel"));
                return Line::from(spans);
            }
            Some(Open::Completion) => {
                spans.extend(key("tab", "take"));
                spans.extend(key("enter", "take and send"));
                spans.extend(key("↑↓", "move"));
                spans.extend(key("esc", "close"));
                return Line::from(spans);
            }
            None => {}
        }
        // First while it is there: what to do with a selection is the question the person
        // has just now, and the keys behind it are the ones they always have.
        if !self.mouse_ours {
            spans.extend(key("ctrl-t", "mouse back"));
        }
        if self.selected {
            spans.extend(key("ctrl-y", "take it"));
            spans.extend(key("esc", "let go"));
        }
        spans.extend(key(
            "shift-tab",
            match (self.planning, self.accepting_writes) {
                (true, _) => "act",
                (false, false) => "write",
                (false, true) => "plan",
            },
        ));
        if self.finding.is_some() {
            spans.extend(key("enter", "next"));
            spans.extend(key("↑", "previous"));
            spans.extend(key("esc", "stop looking"));
            return Line::from(spans);
        }
        if self.busy {
            spans.extend(key("esc", "stop"));
            spans.extend(key("enter", "steer"));
            spans.extend(key("alt-enter", "queue"));
        } else if self.pointing {
            spans.extend(key("alt-↑↓", "steps"));
            if self.on_receipt {
                spans.extend(key("u", "put back"));
                spans.extend(key("d", "diff"));
            }
            spans.extend(key("space", "unfold"));
            spans.extend(key("esc", "back"));
        } else {
            spans.extend(key("enter", "send"));
            spans.extend(key("alt-↑", "steps"));
            spans.extend(key("ctrl-f", "find"));
            if self.rail_offered {
                spans.extend(key("ctrl-b", "rail"));
            }
            spans.extend(key("/help", "more"));
        }
        // What the key does here, which is not the same thing twice: a turn in flight is
        // stopped, a line being typed is cleared, and only an empty line is offered the
        // way out. A hint that said "quit" throughout was wrong two thirds of the time.
        spans.extend(key(
            "ctrl-c",
            match (self.busy, self.typing) {
                (true, _) => "stop",
                (false, true) => "clear",
                (false, false) => "leave",
            },
        ));
        Line::from(spans)
    }
}

#[cfg(test)]
mod tests {
    /// A narrow window drops keys deliberately and says it did.
    ///
    /// The terminal cutting the line loses whichever key is last, without a mark, which is
    /// the same as not having written it - and the whole reason this line exists is that a
    /// shortcut nobody can see is one nobody uses.
    #[test]
    fn the_keys_fit_the_width_they_are_given() {
        let s = status(false, None, Duration::from_secs(0));
        let width = |line: &Line<'static>| -> usize {
            line.spans.iter().map(|x| x.content.chars().count()).sum()
        };
        let whole = s.keys();
        // Given room, nothing is dropped and nothing is added.
        assert_eq!(width(&s.keys_within(width(&whole))), width(&whole));
        // Given less, what is shown fits and says there is more.
        for room in [10, 24, 40, 60] {
            let cut = s.keys_within(room);
            assert!(width(&cut) <= room, "at {room}: {}", width(&cut));
            assert!(
                cut.spans.last().is_some_and(|x| x.content.contains('…')),
                "at {room} nothing said the list goes on"
            );
        }
    }

    use super::*;

    fn status(busy: bool, phase: Option<&str>, in_phase: Duration) -> Status {
        Status {
            planning: false,
            pointing: false,
            typing: false,
            selected: false,
            mouse_ours: true,
            queued: 0,
            finding: None,
            rate: None,
            background: None,
            said: None,
            accepting_writes: false,
            context_percent: 40,
            plan: None,
            rail_offered: false,
            attaching: None,
            on_receipt: false,
            open: None,
            step: 2,
            max_steps: 16,
            busy,
            phase: phase.map(str::to_string),
            in_phase,
            tick: 1,
        }
    }

    /// A bare percentage is a number nobody can act on, so it says what it measures.
    #[test]
    fn the_context_figure_says_what_it_is() {
        let line = status(false, None, Duration::default())
            .to_line()
            .to_string();
        assert!(line.contains("ctx 40%"), "{line}");
    }

    #[test]
    fn a_busy_turn_says_what_it_is_doing_and_for_how_long() {
        let line = status(
            true,
            Some("waiting for the model"),
            Duration::from_millis(8400),
        )
        .to_line()
        .to_string();
        assert!(line.contains("waiting for the model 8.4s"), "{line}");
        // The spinner takes the place of the dot, so something moves every tick.
        assert!(line.starts_with(" ◓ "), "{line}");
    }

    #[test]
    fn a_long_wait_reads_as_minutes() {
        let line = status(true, Some("running bash"), Duration::from_secs(247))
            .to_line()
            .to_string();
        assert!(line.contains("running bash 4m07s"), "{line}");
    }

    #[test]
    fn an_idle_status_says_nothing_of_a_phase() {
        let line = status(false, Some("answering"), Duration::from_secs(3))
            .to_line()
            .to_string();
        assert!(!line.contains("answering"), "{line}");
        assert!(line.starts_with("   "), "{line}");
    }
}
