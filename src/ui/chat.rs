use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// A single rendered entry in the chat scrollback.
#[derive(Debug, Clone)]
pub enum Entry {
    User(String),
    Assistant(String),
    AssistantStreaming(String),
    ToolCall { name: String, args: String },
    ToolResult { name: String, is_error: bool, body: String },
    Notice(String),
}

impl Entry {
    pub fn to_lines(&self) -> Vec<Line<'static>> {
        match self {
            Entry::User(s) => prefix_block("you", Color::Cyan, s),
            Entry::Assistant(s) | Entry::AssistantStreaming(s) => {
                prefix_block("assistant", Color::Green, s)
            }
            Entry::ToolCall { name, args } => prefix_block(
                &format!("→ {name}"),
                Color::Yellow,
                &truncate(args, 800),
            ),
            Entry::ToolResult { name, is_error, body } => {
                let label = if *is_error {
                    format!("← {name} [error]")
                } else {
                    format!("← {name}")
                };
                prefix_block(
                    &label,
                    if *is_error { Color::Red } else { Color::Magenta },
                    &truncate(body, 1600),
                )
            }
            Entry::Notice(s) => prefix_block("·", Color::DarkGray, s),
        }
    }
}

fn prefix_block(label: &str, color: Color, body: &str) -> Vec<Line<'static>> {
    let header = Line::from(Span::styled(
        format!("[{label}]"),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    ));
    let mut out = vec![header];
    for l in body.lines() {
        out.push(Line::from(Span::raw(l.to_string())));
    }
    out.push(Line::from(""));
    out
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n { return s.to_string(); }
    let mut t = s.chars().take(n).collect::<String>();
    t.push_str("\n… (truncated)");
    t
}
