//! What a command has to say, on a page of its own.
//!
//! A command's answer is not part of the conversation: the model did not say it, it will
//! not be sent anywhere, and it is read once. Poured into the thread it stayed there for
//! the rest of the session, sixty-eight lines of model catalogue between two questions.
//!
//! So it goes on a page over the conversation, scrolls, and closes leaving nothing.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};
use ratatui::Frame;

/// A page of text over the conversation.
pub struct Sheet {
    title: String,
    lines: Vec<String>,
    at: usize,
}

impl Sheet {
    pub fn new(title: impl Into<String>, body: &str) -> Self {
        Self {
            title: title.into(),
            lines: body.lines().map(str::to_string).collect(),
            at: 0,
        }
    }

    /// The text as it was, for taking it out with the rest.
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn scroll(&mut self, by: isize, height: usize) {
        let last = self.lines.len().saturating_sub(height.max(1));
        let at = (self.at as isize + by).clamp(0, last as isize);
        self.at = at as usize;
    }

    pub fn render(&mut self, f: &mut Frame, area: Rect) {
        // A title line, the body, and a line saying how to leave: a page with no way out
        // written on it is a page somebody has to guess their way out of.
        let height = area.height.saturating_sub(2) as usize;
        let last = self.lines.len().saturating_sub(height.max(1));
        self.at = self.at.min(last);
        let mut out = vec![Line::from(Span::styled(
            self.title.clone(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))];
        out.extend(
            self.lines
                .iter()
                .skip(self.at)
                .take(height)
                .map(|l| Line::from(l.clone())),
        );
        let more = self.lines.len().saturating_sub(self.at + height);
        out.push(Line::from(Span::styled(
            match more {
                0 => "esc closes".to_string(),
                n => format!("esc closes   ↑↓ scroll   {n} more line(s)"),
            },
            Style::default().fg(Color::DarkGray),
        )));
        f.render_widget(Clear, area);
        f.render_widget(Paragraph::new(out).wrap(Wrap { trim: false }), area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrolling_stops_at_both_ends() {
        let mut s = Sheet::new(
            "t",
            &(1..=20)
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        );
        s.scroll(-5, 10);
        assert_eq!(s.at, 0);
        s.scroll(100, 10);
        assert_eq!(s.at, 10);
        s.scroll(1, 10);
        assert_eq!(s.at, 10);
    }

    /// A page shorter than the room it is given does not scroll at all.
    #[test]
    fn a_short_page_stays_put() {
        let mut s = Sheet::new("t", "one\ntwo");
        s.scroll(5, 10);
        assert_eq!(s.at, 0);
    }

    #[test]
    fn the_text_comes_back_as_it_went_in() {
        let s = Sheet::new("t", "one\ntwo");
        assert_eq!(s.text(), "one\ntwo");
    }
}
