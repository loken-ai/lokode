use std::time::Duration;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

#[derive(Debug, Clone)]
pub struct Status {
    pub model: String,
    pub root: String,
    /// Shown only while planning: acting is the ordinary state and does not need saying.
    pub planning: bool,
    /// How full the history budget is. Coloured once it is close, because the first
    /// elision is the moment a session starts forgetting what it was told.
    pub context_percent: u8,
    pub step: usize,
    pub max_steps: usize,
    pub busy: bool,
    /// What the turn is doing, when it is doing something that prints nothing.
    pub phase: Option<String>,
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
        spans.push(Span::styled(
            match self.planning {
                true => " PLAN ",
                false => " ACT  ",
            },
            Style::default()
                .fg(Color::Black)
                .bg(if self.planning {
                    Color::Cyan
                } else {
                    Color::Green
                })
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
        spans.push(Span::styled(
            format!("  {}%", self.context_percent),
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
        spans.extend(key(
            "shift-tab",
            match self.planning {
                true => "act",
                false => "plan",
            },
        ));
        if self.busy {
            spans.extend(key("esc", "stop"));
            spans.extend(key("enter", "steer"));
        } else {
            spans.extend(key("enter", "send"));
            spans.extend(key("pgup", "scroll"));
            spans.extend(key("/copy", "take it"));
            spans.extend(key("/help", "more"));
        }
        spans.extend(key("ctrl-c", "quit"));
        Line::from(spans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(busy: bool, phase: Option<&str>, in_phase: Duration) -> Status {
        Status {
            model: "qwen3:8b".into(),
            root: "/p".into(),
            planning: false,
            context_percent: 40,
            step: 2,
            max_steps: 16,
            busy,
            phase: phase.map(str::to_string),
            in_phase,
            tick: 1,
        }
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
