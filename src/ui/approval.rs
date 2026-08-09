use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::sandbox::approval::ApprovalRequest;

pub struct ApprovalView<'a> {
    pub req: &'a ApprovalRequest,
}

impl<'a> ApprovalView<'a> {
    pub fn render(&self, f: &mut Frame, area: Rect) {
        let area = centered(80, 70, area);
        f.render_widget(Clear, area);

        let block = Block::default().borders(Borders::ALL).title(Span::styled(
            format!(" approval required: {} ", self.req.tool),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
        f.render_widget(block.clone(), area);

        let inner = block.inner(area);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(1),
                Constraint::Length(2),
            ])
            .split(inner);

        let summary = Paragraph::new(self.req.summary.clone())
            .style(Style::default().fg(Color::White))
            .wrap(Wrap { trim: false });
        f.render_widget(summary, chunks[0]);

        let body = self
            .req
            .diff_preview
            .clone()
            .unwrap_or_else(|| "(no preview)".into());
        let preview = Paragraph::new(colorise_diff(&body)).wrap(Wrap { trim: false });
        f.render_widget(preview, chunks[1]);

        // What `Always` covers is spelled out rather than left to the word: it used to
        // mean the whole tool, and a user pressing it while looking at one harmless
        // command had no way to know that.
        let footer = Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    "[Y]es",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(
                    "[N]o",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled("[A]lways", Style::default().fg(Color::Cyan)),
                Span::raw("  "),
                Span::styled("Esc", Style::default().fg(Color::Red)),
                Span::raw(" declines"),
            ]),
            Line::from(Span::styled(
                format!(
                    "Always covers {} for this session",
                    self.req.scope.describe()
                ),
                Style::default().fg(Color::DarkGray),
            )),
        ])
        .alignment(Alignment::Center);
        f.render_widget(footer, chunks[2]);
    }
}

fn colorise_diff(s: &str) -> Vec<Line<'static>> {
    s.lines()
        .map(|l| {
            let style = if l.starts_with('+') && !l.starts_with("+++") {
                Style::default().fg(Color::Green)
            } else if l.starts_with('-') && !l.starts_with("---") {
                Style::default().fg(Color::Red)
            } else if l.starts_with("@@") {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default()
            };
            Line::from(Span::styled(l.to_string(), style))
        })
        .collect()
}

fn centered(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(v[1])[1]
}
