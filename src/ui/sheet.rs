//! What a command has to say, on a page of its own.
//!
//! A command's answer is not part of the conversation: the model did not say it, it will
//! not be sent anywhere, and it is read once. So it goes on a page over the conversation,
//! scrolls, and closes leaving nothing.
//!
//! The page is a surface of its own: a title band naming the command, the answer drawn as
//! what it is (markdown, a diff, the window's map, or lines of text), and a foot that says
//! how to move and how much is left.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

use crate::agent::session::Occupancy;
use crate::ui::chat::wrap;
use crate::ui::theme::theme;

/// The columns left empty on each side of the page's text.
const MARGIN: usize = 2;

/// What a page holds.
pub enum Body {
    /// Lines of text, each kept as written.
    Text(String),
    /// Markdown, read the way the model's answers are.
    Markdown(String),
    /// A unified diff, numbered and coloured.
    Diff(String),
    /// What fills the window, drawn as a map.
    Window(Occupancy),
}

/// A page over the conversation.
pub struct Sheet {
    /// What names the page in its band.
    badge: String,
    subtitle: String,
    body: Body,
    /// The rows as last drawn, and the width they were drawn for.
    drawn: Option<(usize, Vec<Line<'static>>)>,
    /// How many body rows fit, as last drawn.
    height: usize,
    at: usize,
}

/// What a command's page is about, under its name.
fn subtitle_of(verb: &str) -> &'static str {
    match verb {
        "help" => "commands and keys",
        "context" => "what fills the window",
        "cost" => "what this session spent",
        "approve" => "what is settled and agreed",
        "tools" => "what the model can call",
        "diff" => "what this session changed",
        "why" => "why this model answers",
        "digest" => "what is known of this project",
        "mcp" => "the tool servers",
        "undo" => "what was put back",
        _ => "",
    }
}

impl Sheet {
    /// The page a command's answer makes, drawn as what it is: a diff as a diff, the answer
    /// of a command that writes markdown as markdown, anything else as lines of text.
    pub fn new(verb: &str, body: &str) -> Self {
        let body = if crate::ui::chat::looks_like_diff(body) {
            Body::Diff(body.to_string())
        } else if matches!(verb, "help" | "cost" | "approve" | "tools" | "mcp")
            || body.contains("\n## ")
            || body.starts_with("## ")
        {
            Body::Markdown(body.to_string())
        } else {
            Body::Text(body.to_string())
        };
        Self::with(verb, body)
    }

    /// A page holding `body`, under the command `verb`.
    pub fn with(verb: &str, body: Body) -> Self {
        Self {
            badge: format!("/{verb}"),
            subtitle: subtitle_of(verb).to_string(),
            body,
            drawn: None,
            height: 0,
            at: 0,
        }
    }

    /// A page that is not a command's, under its own name and what it is about.
    pub fn titled(name: &str, about: &str, body: Body) -> Self {
        Self {
            badge: name.to_string(),
            subtitle: about.to_string(),
            body,
            drawn: None,
            height: 0,
            at: 0,
        }
    }

    /// The text as it was, for taking it out with the rest.
    pub fn text(&self) -> String {
        match &self.body {
            Body::Text(s) | Body::Markdown(s) | Body::Diff(s) => s.clone(),
            Body::Window(w) => w
                .parts
                .iter()
                .map(|(name, chars)| format!("{name}: {chars} characters"))
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    pub fn scroll(&mut self, by: isize, height: usize) {
        let rows = self.drawn.as_ref().map_or(0, |(_, r)| r.len());
        let room = match self.height {
            0 => height.max(1),
            h => h,
        };
        let last = rows.saturating_sub(room);
        self.at = (self.at as isize + by).clamp(0, last as isize) as usize;
    }

    /// The body's rows for a page `width` columns wide, drawn again only when the width
    /// changes.
    fn rows(&mut self, width: usize) -> &[Line<'static>] {
        if self.drawn.as_ref().map(|(w, _)| *w) != Some(width) {
            let rows = match &self.body {
                Body::Text(s) => text_rows(s, width),
                Body::Markdown(s) => crate::ui::markdown::render(s, width),
                Body::Diff(s) => crate::ui::diff::rows(s, width),
                Body::Window(w) => window_rows(w, width),
            };
            self.drawn = Some((width, rows));
        }
        &self.drawn.as_ref().expect("drawn just above").1
    }

    pub fn render(&mut self, f: &mut Frame, area: Rect) {
        let t = theme();
        let surface = match t.tint {
            Some(tint) => Style::default().bg(tint),
            None => Style::default(),
        };
        let inner = (area.width as usize).saturating_sub(MARGIN * 2);
        // A title band, a blank row, the body, a blank row and the foot.
        let height = (area.height as usize).saturating_sub(4).max(1);
        self.height = height;
        let badge = self.badge.clone();
        let subtitle = self.subtitle.clone();
        let total = self.rows(inner).len();
        self.at = self.at.min(total.saturating_sub(height));
        let at = self.at;
        let margin = " ".repeat(MARGIN);

        let mut out = vec![Line::from(vec![
            Span::raw(margin.clone()),
            Span::styled(
                format!(" {badge} "),
                Style::default()
                    .fg(t.ink)
                    .bg(t.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  {subtitle}"), t.dim),
        ])];
        out.push(Line::from(""));
        for row in self.rows(inner).iter().skip(at).take(height) {
            let mut spans = vec![Span::raw(margin.clone())];
            spans.extend(row.spans.iter().cloned());
            out.push(Line::from(spans));
        }
        while out.len() < height + 2 {
            out.push(Line::from(""));
        }
        out.push(Line::from(""));
        let mut foot = vec![
            Span::raw(margin.clone()),
            Span::styled("esc", t.key),
            Span::styled(" close", t.dim),
        ];
        if total > height {
            foot.push(Span::styled("  ↑↓", t.key));
            foot.push(Span::styled(" scroll", t.dim));
            foot.push(Span::styled("  pgup pgdn", t.key));
            foot.push(Span::styled(" page", t.dim));
            let shown = format!("{}-{} of {total}", at + 1, (at + height).min(total));
            let used: usize = foot.iter().map(|s| s.content.chars().count()).sum();
            let gap = (area.width as usize).saturating_sub(used + shown.chars().count() + MARGIN);
            foot.push(Span::raw(" ".repeat(gap.max(2))));
            foot.push(Span::styled(shown, t.dim));
        }
        out.push(Line::from(foot));
        f.render_widget(Clear, area);
        f.render_widget(Paragraph::new(out).style(surface), area);
    }
}

/// Lines of text, each kept as written and wrapped to `width`. The first line is the
/// answer and stands out, a line that ends in a colon heads what follows, and an indented
/// `name: value` has its name receding.
fn text_rows(s: &str, width: usize) -> Vec<Line<'static>> {
    let t = theme();
    let mut out = Vec::new();
    for (i, raw) in s.lines().enumerate() {
        let line = if i == 0 {
            Line::from(Span::styled(
                raw.to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ))
        } else if raw.ends_with(':') && !raw.starts_with(' ') {
            Line::from(Span::styled(raw.to_string(), t.heading))
        } else if let Some((name, value)) = raw
            .strip_prefix("  ")
            .and_then(|rest| rest.split_once(": "))
            .filter(|(name, _)| !name.contains(' ') || name.len() < 24)
        {
            Line::from(vec![
                Span::styled(format!("  {name}: "), t.dim),
                Span::raw(value.to_string()),
            ])
        } else {
            Line::from(raw.to_string())
        };
        out.extend(wrap(vec![line], width));
    }
    out
}

/// What fills the window: how full it is, the bar of who filled it as wide as the page,
/// and each part with its share.
fn window_rows(w: &Occupancy, width: usize) -> Vec<Line<'static>> {
    let t = theme();
    let percent_style = match w.percent {
        0..=74 => Style::default().add_modifier(Modifier::BOLD),
        75..=99 => Style::default().fg(t.warn).add_modifier(Modifier::BOLD),
        _ => Style::default().fg(t.bad).add_modifier(Modifier::BOLD),
    };
    let mut out = vec![
        Line::from(vec![
            Span::styled(format!("{}%", w.percent), percent_style),
            Span::styled(
                format!(
                    " of the history budget, {} of {} characters",
                    crate::ui::chat::tokens(w.used as u64),
                    crate::ui::chat::tokens(w.budget as u64)
                ),
                t.dim,
            ),
        ]),
        Line::from(""),
    ];
    let cells = width.saturating_sub(1);
    let mut bar = crate::ui::rail::window_bar(w, cells);
    bar.spans.remove(0);
    out.push(bar);
    // A scale under the bar: where a quarter, a half, three quarters and all of it fall.
    let mut scale: Vec<char> = vec![' '; cells];
    for (share, mark) in [
        (0, "0"),
        (25, "25%"),
        (50, "50%"),
        (75, "75%"),
        (100, "100%"),
    ] {
        let at = (cells.saturating_sub(1) * share / 100).min(cells.saturating_sub(mark.len()));
        for (k, c) in mark.chars().enumerate() {
            if let Some(slot) = scale.get_mut(at + k) {
                *slot = c;
            }
        }
    }
    out.push(Line::from(Span::styled(
        scale.into_iter().collect::<String>(),
        t.dim,
    )));
    out.push(Line::from(""));
    let budget = w.budget.max(1);
    for (at, (name, chars)) in w.parts.iter().enumerate() {
        out.push(Line::from(vec![
            Span::styled("██ ", Style::default().fg(crate::ui::rail::part_colour(at))),
            Span::raw(format!("{name:<14}")),
            Span::styled(
                format!(
                    "{:>8} characters  {:>3}%",
                    crate::ui::chat::tokens(*chars as u64),
                    chars * 100 / budget
                ),
                t.dim,
            ),
        ]));
    }
    out.push(Line::from(""));
    out.push(Line::from(Span::styled(
        "Past a hundred per cent the oldest steps are elided. /compact does it now.",
        t.dim,
    )));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::chat::line_text;

    fn drawn(s: &mut Sheet, width: usize) -> Vec<String> {
        s.rows(width).iter().map(line_text).collect()
    }

    #[test]
    fn scrolling_stops_at_both_ends() {
        let mut s = Sheet::new(
            "t",
            &(1..=20)
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        );
        drawn(&mut s, 40);
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
        drawn(&mut s, 40);
        s.scroll(5, 10);
        assert_eq!(s.at, 0);
    }

    #[test]
    fn the_text_comes_back_as_it_went_in() {
        let s = Sheet::new("t", "one\ntwo");
        assert_eq!(s.text(), "one\ntwo");
    }

    /// A command that writes markdown gets its tables drawn, and plain text keeps its lines.
    #[test]
    fn markdown_is_drawn_and_text_keeps_its_lines() {
        let mut help = Sheet::new("help", "| Key | What |\n|---|---|\n| esc | close |");
        assert_eq!(drawn(&mut help, 40), vec!["  Key  What", "  esc  close"]);
        let mut why = Sheet::new("why", "qwen3 answers\nby kind of work:\n  edit: qwen3");
        assert_eq!(
            drawn(&mut why, 40),
            vec!["qwen3 answers", "by kind of work:", "  edit: qwen3"]
        );
    }

    /// A diff is numbered, whatever command produced it.
    #[test]
    fn a_diff_is_drawn_as_one() {
        let mut s = Sheet::new("diff", "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new");
        let rows = drawn(&mut s, 40);
        assert_eq!(rows[0], "x");
        assert!(rows.iter().any(|r| r.starts_with("  1 + new")), "{rows:?}");
    }

    /// The window's map says how full it is and gives each part its share.
    #[test]
    fn the_window_is_drawn_as_a_map() {
        let mut s = Sheet::with(
            "context",
            Body::Window(Occupancy {
                percent: 23,
                used: 2300,
                budget: 10_000,
                parts: vec![("instructions", 1000), ("tools", 1300)],
            }),
        );
        let rows = drawn(&mut s, 60);
        assert!(rows[0].starts_with("23% of the history budget"), "{rows:?}");
        assert!(
            rows.iter()
                .any(|r| r.contains("tools") && r.ends_with("13%")),
            "{rows:?}"
        );
    }

    /// The page is a band naming the command, a blank row, the body and a foot.
    #[test]
    fn a_page_has_a_band_a_body_and_a_foot() {
        use ratatui::backend::TestBackend;
        let mut s = Sheet::new(
            "help",
            "## Commands\n\n| Command | What |\n|---|---|\n| `/why` | why |",
        );
        let mut terminal = ratatui::Terminal::new(TestBackend::new(40, 10)).expect("a terminal");
        terminal.draw(|f| s.render(f, f.area())).expect("a frame");
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..10)
            .map(|y| {
                (0..40)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                "   /help   commands and keys",
                "",
                "  Commands",
                "",
                "    Command  What",
                "    /why     why",
                "",
                "",
                "",
                "  esc close",
            ]
        );
    }
}
