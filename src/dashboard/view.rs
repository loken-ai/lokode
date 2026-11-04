//! The dashboard in the terminal: one line for each session of each server.
//!
//! The lines are made from a snapshot of the fleet, so what is drawn can be tested without a
//! terminal.

use std::time::{Duration, Instant};

use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};
use ratatui::Frame;

use super::client::Fleet;
use super::state::Status;

/// One line of the dashboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// The server the line belongs to.
    pub node: String,
    /// The session, or `None` for a server that could not be reached.
    pub session: Option<String>,
    pub project: String,
    pub status: Option<Status>,
    /// Whether the server is connected.
    pub reached: bool,
    pub doing: String,
    pub plan: String,
    pub quiet: String,
    /// The first question the session waits on, and what answering always to it covers.
    pub ask: Option<(i64, String)>,
}

/// The lines of `fleet` at `now`: sessions that wait for the person first, then those that work,
/// then the idle ones, and the servers that could not be reached last.
pub fn lines(fleet: &Fleet, now: Instant) -> Vec<Item> {
    let mut lines = Vec::new();
    for (node, entry) in &fleet.servers {
        let project = std::path::Path::new(&entry.state.root)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| entry.addr.to_string());
        if entry.state.sessions.is_empty() {
            lines.push(Item {
                node: node.clone(),
                session: None,
                project: project.clone(),
                status: None,
                reached: entry.link.is_some(),
                doing: entry.error.clone().unwrap_or_default(),
                plan: String::new(),
                quiet: String::new(),
                ask: None,
            });
        }
        for (id, s) in &entry.state.sessions {
            let doing = match (s.asks.values().next(), &s.tool, s.said.is_empty()) {
                (Some(ask), _, _) => format!("asks: {}", ask.title),
                (None, Some(tool), _) if s.status() == Status::Working => tool.clone(),
                (None, _, false) => s.said.replace('\n', " "),
                _ => s.stopped.clone().unwrap_or_default(),
            };
            lines.push(Item {
                node: node.clone(),
                session: Some(id.clone()),
                project: project.clone(),
                status: Some(s.status()),
                reached: true,
                doing,
                plan: match s.plan {
                    (_, 0) => String::new(),
                    (done, all) => format!("{done}/{all}"),
                },
                quiet: s
                    .active
                    .map(|at| ago(now.saturating_duration_since(at)))
                    .unwrap_or_default(),
                ask: s.asks.iter().next().map(|(id, a)| (*id, a.always.clone())),
            });
        }
    }
    lines.sort_by(|a, b| {
        let rank = |l: &Item| l.status.map_or(3, |s| s as u8);
        rank(a)
            .cmp(&rank(b))
            .then_with(|| a.project.cmp(&b.project))
    });
    lines
}

/// A length of time as a person says it.
pub fn ago(time: Duration) -> String {
    match time.as_secs() {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s => format!("{}h", s / 3600),
    }
}

fn colour(status: Option<Status>, reached: bool, doing: &str) -> Color {
    match (status, reached) {
        (Some(Status::Waiting), _) => Color::Yellow,
        (Some(Status::Working), _) => Color::Green,
        (Some(Status::Idle), _) | (None, true) => Color::DarkGray,
        (None, false) if doing.is_empty() => Color::DarkGray,
        (None, false) => Color::Red,
    }
}

/// Draw `lines`, with the one at `selected` marked.
pub fn draw(frame: &mut Frame, lines: &[Item], selected: usize, note: &str) {
    let [table_area, help_area] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(frame.area());
    let header = Row::new(["", "Project", "Session", "State", "Doing", "Plan", "Quiet"])
        .style(Style::new().add_modifier(Modifier::BOLD));
    let rows = lines.iter().map(|l| {
        let state = match (l.status, l.reached) {
            (Some(status), _) => status.label(),
            (None, true) => "no session",
            // No reason yet means the server is still being reached.
            (None, false) if l.doing.is_empty() => "connecting",
            (None, false) => "unreachable",
        };
        Row::new([
            Cell::from("●").style(Style::new().fg(colour(l.status, l.reached, &l.doing))),
            Cell::from(l.project.clone()),
            Cell::from(
                l.session
                    .as_deref()
                    .map(|s| s.chars().take(8).collect::<String>())
                    .unwrap_or_default(),
            ),
            Cell::from(state).style(Style::new().fg(colour(l.status, l.reached, &l.doing))),
            Cell::from(l.doing.clone()),
            Cell::from(l.plan.clone()),
            Cell::from(l.quiet.clone()),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(2),
            Constraint::Length(20),
            Constraint::Length(9),
            Constraint::Length(12),
            Constraint::Fill(1),
            Constraint::Length(6),
            Constraint::Length(6),
        ],
    )
    .header(header)
    .block(
        Block::new()
            .borders(Borders::ALL)
            .title(" lokode dashboard "),
    )
    .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    let mut state = TableState::default().with_selected((!lines.is_empty()).then_some(selected));
    frame.render_stateful_widget(table, table_area, &mut state);
    let help = Line::from(vec![
        Span::styled("↑↓", Style::new().add_modifier(Modifier::BOLD)),
        Span::raw(" select  "),
        Span::styled("enter", Style::new().add_modifier(Modifier::BOLD)),
        Span::raw(" open  "),
        Span::styled("m", Style::new().add_modifier(Modifier::BOLD)),
        Span::raw(" message  "),
        Span::styled("x", Style::new().add_modifier(Modifier::BOLD)),
        Span::raw(" stop  "),
        Span::styled("q", Style::new().add_modifier(Modifier::BOLD)),
        Span::raw(" quit  "),
        Span::raw(note.to_string()),
    ]);
    frame.render_widget(Paragraph::new(help), help_area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::client::Entry;
    use crate::dashboard::state::SessionState;
    use serde_json::json;

    fn entry(root: &str, sessions: Vec<SessionState>) -> Entry {
        let mut entry = Entry {
            addr: "192.0.2.1:4000".parse().unwrap(),
            state: Default::default(),
            link: None,
            error: None,
        };
        entry.state.root = root.to_string();
        for s in sessions {
            entry.state.sessions.insert(s.id.clone(), s);
        }
        entry
    }

    fn working(id: &str, now: Instant) -> SessionState {
        let mut s = SessionState::new(id);
        s.update(
            &json!({"sessionUpdate": "tool_call", "title": "$ cargo build"}),
            now,
        );
        s
    }

    #[test]
    fn sessions_that_wait_come_first_and_unreachable_servers_last() {
        let now = Instant::now();
        let mut waiting = working("w", now);
        waiting.asks.insert(
            1,
            crate::dashboard::state::Ask {
                title: "$ rm -r build".into(),
                always: String::new(),
            },
        );
        let mut fleet = Fleet::default();
        fleet.servers.insert(
            "a".into(),
            entry(
                "/home/me/api",
                vec![SessionState::new("i"), working("k", now)],
            ),
        );
        fleet
            .servers
            .insert("b".into(), entry("/home/me/web", vec![waiting]));
        let mut gone = entry("", vec![]);
        gone.error = Some("it did not answer".into());
        fleet.servers.insert("c".into(), gone);

        let lines = lines(&fleet, now + Duration::from_secs(90));
        let order: Vec<_> = lines.iter().map(|l| (l.node.as_str(), l.status)).collect();
        assert_eq!(
            order,
            [
                ("b", Some(Status::Waiting)),
                ("a", Some(Status::Working)),
                ("a", Some(Status::Idle)),
                ("c", None),
            ]
        );
        assert_eq!(lines[0].doing, "asks: $ rm -r build");
        assert_eq!(lines[1].doing, "$ cargo build");
        assert_eq!(lines[1].quiet, "1m");
        assert_eq!(lines[3].doing, "it did not answer");
    }

    #[test]
    fn a_time_is_said_in_the_unit_that_fits() {
        assert_eq!(ago(Duration::from_secs(5)), "5s");
        assert_eq!(ago(Duration::from_secs(300)), "5m");
        assert_eq!(ago(Duration::from_secs(7300)), "2h");
    }

    #[test]
    fn the_dashboard_is_drawn() {
        let now = Instant::now();
        let mut fleet = Fleet::default();
        fleet.servers.insert(
            "a".into(),
            entry("/home/me/api", vec![working("abcdef012345", now)]),
        );
        let lines = lines(&fleet, now);
        let backend = ratatui::backend::TestBackend::new(100, 8);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &lines, 0, "")).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("api") && text.contains("working") && text.contains("abcdef01"));
    }
}
