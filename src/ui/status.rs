use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

#[derive(Debug, Clone)]
pub struct Status {
    pub model: String,
    pub root: String,
    pub session: String,
    pub grammar: bool,
    pub step: usize,
    pub max_steps: usize,
    pub busy: bool,
}

impl Status {
    pub fn to_line(&self) -> Line<'static> {
        let busy = if self.busy { "●" } else { " " };
        let grammar = if self.grammar { "grammar" } else { "no-grammar" };
        Line::from(vec![
            Span::styled(
                format!(" {busy} "),
                Style::default().fg(if self.busy { Color::Yellow } else { Color::Gray }),
            ),
            Span::raw(format!("model={}  ", self.model)),
            Span::raw(format!("root={}  ", self.root)),
            Span::raw(format!("session={}…  ", &self.session[..self.session.len().min(8)])),
            Span::styled(format!("{grammar}  "), Style::default().fg(Color::Cyan)),
            Span::raw(format!("step {}/{}", self.step, self.max_steps)),
        ])
    }
}
