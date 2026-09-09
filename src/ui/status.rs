use std::time::Duration;

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

#[derive(Debug, Clone)]
pub struct Status {
    pub model: String,
    pub root: String,
    pub session: String,
    pub grammar: bool,
    /// Shown only while planning: acting is the ordinary state and does not need saying.
    pub planning: bool,
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
    pub fn to_line(&self) -> Line<'static> {
        let mark = if self.busy {
            SPINNER[self.tick % SPINNER.len()]
        } else {
            " "
        };
        let grammar = if self.grammar {
            "grammar"
        } else {
            "no-grammar"
        };
        let mut spans = vec![
            Span::styled(
                format!(" {mark} "),
                Style::default().fg(if self.busy {
                    Color::Yellow
                } else {
                    Color::Gray
                }),
            ),
            Span::raw(format!("model={}  ", self.model)),
            Span::raw(format!("root={}  ", self.root)),
            Span::raw(format!(
                "session={}…  ",
                &self.session[..self.session.len().min(8)]
            )),
            Span::styled(format!("{grammar}  "), Style::default().fg(Color::Cyan)),
            Span::raw(format!("step {}/{}", self.step, self.max_steps)),
        ];
        if self.planning {
            spans.insert(
                1,
                Span::styled(
                    "PLAN  ",
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(ratatui::style::Modifier::BOLD),
                ),
            );
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(busy: bool, phase: Option<&str>, in_phase: Duration) -> Status {
        Status {
            model: "qwen3:8b".into(),
            root: "/p".into(),
            session: "0123456789".into(),
            grammar: true,
            planning: false,
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
