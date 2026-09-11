use std::time::Duration;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

#[derive(Debug, Clone)]
pub struct Status {
    pub model: String,
    pub root: String,
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
    pub step: usize,
    pub max_steps: usize,
    pub busy: bool,
    /// Whether there is something in the input, which changes what leaving means.
    pub typing: bool,
    /// Whether something is selected in the pane, which puts taking it on the line.
    pub selected: bool,
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
const SPINNER: [&str; 8] = ["|", "/", "-", "\\", "|", "/", "-", "\\"];

/// Seconds shown to a tenth up to a minute, then as minutes and seconds: a turn that has
/// been waiting four minutes should not read as 247.3.
fn how_long(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s < 60.0 {
        format!("{s:.1}s")
    } else {
        format!("{}m{:02}s", (s / 60.0) as u64, (s % 60.0) as u64)
    }
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
                Color::Yellow
            } else {
                Color::DarkGray
            }),
        ));
        let (badge, colour) = match (self.planning, self.accepting_writes) {
            (true, _) => (" PLAN ", Color::Cyan),
            // Loudest of the three, because it is the one where things happen without
            // being shown first.
            (false, true) => (" WRITE", Color::Yellow),
            (false, false) => (" ACT  ", Color::Green),
        };
        spans.push(Span::styled(
            badge,
            Style::default()
                .fg(Color::Black)
                .bg(colour)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            self.model.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!("  {}", self.root),
            Style::default().fg(Color::DarkGray),
        ));
        // Named, because a bare percentage on a status bar is a number nobody can act
        // on: this one says how close the conversation is to the point where its oldest
        // steps start being dropped to make room.
        spans.push(Span::styled(
            format!("  ctx {}%", self.context_percent),
            Style::default().fg(match self.context_percent {
                0..=74 => Color::DarkGray,
                75..=99 => Color::Yellow,
                _ => Color::Red,
            }),
        ));
        if self.busy {
            spans.push(Span::styled(
                format!("  {}/{}", self.step, self.max_steps),
                Style::default().fg(Color::DarkGray),
            ));
        }
        // The phase is what a user is looking for when the screen has stopped changing, so
        // it goes last, where the eye lands, with the time it has been there beside it.
        if let Some(said) = self.said.as_deref() {
            spans.push(Span::styled(
                format!("  {said}"),
                Style::default().fg(Color::Cyan),
            ));
        }
        if let Some(phase) = self.phase.as_deref().filter(|_| self.busy) {
            spans.push(Span::styled(
                format!("  {phase} {}", how_long(self.in_phase)),
                Style::default().fg(Color::Yellow),
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
        spans.push(Span::styled(" …", Style::default().fg(Color::DarkGray)));
        Line::from(spans)
    }

    pub fn keys(&self) -> Line<'static> {
        let key = |k: &str, what: &str| {
            vec![
                Span::styled(
                    format!("  {k}"),
                    Style::default()
                        .fg(Color::Gray)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!(" {what}"), Style::default().fg(Color::DarkGray)),
            ]
        };
        let mut spans = Vec::new();
        // First while it is there: what to do with a selection is the question the person
        // has just now, and the keys behind it are the ones they always have.
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
        if self.busy {
            spans.extend(key("esc", "stop"));
            spans.extend(key("enter", "steer"));
        } else if self.pointing {
            spans.extend(key("alt-↑↓", "steps"));
            spans.extend(key("space", "unfold"));
            spans.extend(key("esc", "back"));
        } else {
            spans.extend(key("enter", "send"));
            spans.extend(key("alt-↑", "steps"));
            spans.extend(key("ctrl-f", "find"));
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
            model: "qwen3:8b".into(),
            root: "/p".into(),
            planning: false,
            pointing: false,
            typing: false,
            selected: false,
            said: None,
            accepting_writes: false,
            context_percent: 40,
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
        assert!(line.starts_with(" / "), "{line}");
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
