//! A list you choose from, over the input, filtered by what is being typed.
//!
//! Three things used to need it and none had it: the commands, which had to be known by
//! heart and typed without a mistake; the models, which arrived as sixty-eight lines
//! poured into the conversation; and the files, which had to be spelled out in full.
//!
//! One list, three sources. It opens on what is already typed, narrows at every keystroke,
//! and closes leaving nothing behind - a chooser is not part of the conversation.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

/// How many rows the list may take. Enough to choose from, short enough to leave the
/// conversation visible behind it.
const ROWS: usize = 8;

/// One thing that can be chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// What replaces the token being typed.
    pub value: String,
    /// What it is, in a few words. Empty when the value says it.
    pub about: String,
}

impl Choice {
    pub fn new(value: impl Into<String>, about: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            about: about.into(),
        }
    }
}

/// A list open over the input.
pub struct Picker {
    /// Everything that could be chosen, in the order it was offered.
    all: Vec<Choice>,
    /// What the list is narrowed by: the token being typed, without its sigil.
    filter: String,
    /// Where the highlight is, among what the filter leaves.
    at: usize,
    /// The sigil the token started with, so the accepted value goes back with it.
    sigil: char,
}

impl Picker {
    pub fn new(sigil: char, all: Vec<Choice>) -> Self {
        Self {
            all,
            filter: String::new(),
            at: 0,
            sigil,
        }
    }

    pub fn sigil(&self) -> char {
        self.sigil
    }

    /// Narrow to `filter`, keeping the highlight inside what is left.
    pub fn narrow(&mut self, filter: &str) {
        self.filter = filter.to_lowercase();
        self.at = self.at.min(self.shown().len().saturating_sub(1));
    }

    /// What the filter leaves, best first.
    ///
    /// A value that starts with what was typed comes before one that merely contains it:
    /// somebody typing `mo` means `model` before `remodel`.
    pub fn shown(&self) -> Vec<&Choice> {
        let mut starts: Vec<&Choice> = Vec::new();
        let mut contains: Vec<&Choice> = Vec::new();
        for c in &self.all {
            let value = c.value.to_lowercase();
            if self.filter.is_empty() || value.starts_with(&self.filter) {
                starts.push(c);
            } else if value.contains(&self.filter) {
                contains.push(c);
            }
        }
        starts.extend(contains);
        starts
    }

    pub fn is_empty(&self) -> bool {
        self.shown().is_empty()
    }

    pub fn move_by(&mut self, delta: isize) {
        let n = self.shown().len();
        if n == 0 {
            return;
        }
        let at = self.at as isize + delta;
        self.at = at.rem_euclid(n as isize) as usize;
    }

    /// What is highlighted now.
    pub fn chosen(&self) -> Option<String> {
        self.shown().get(self.at).map(|c| c.value.clone())
    }

    /// Draw it just above `below`, growing upwards so the input never moves.
    pub fn render(&self, f: &mut Frame, below: Rect) {
        let shown = self.shown();
        if shown.is_empty() {
            return;
        }
        let rows = shown.len().min(ROWS);
        let top = below.y.saturating_sub(rows as u16);
        let area = Rect {
            x: below.x,
            y: top,
            width: below.width,
            height: rows as u16,
        };
        // The highlight stays in view when the list is longer than the rows allow.
        let first = self.at.saturating_sub(rows.saturating_sub(1));
        let lines: Vec<Line<'static>> = shown
            .iter()
            .enumerate()
            .skip(first)
            .take(rows)
            .map(|(i, c)| {
                let here = i == self.at;
                let mark = if here { "❯ " } else { "  " };
                let value = Style::default().fg(if here { Color::Cyan } else { Color::Gray });
                let value = match here {
                    true => value.add_modifier(Modifier::BOLD),
                    false => value,
                };
                Line::from(vec![
                    Span::styled(mark, value),
                    Span::styled(c.value.clone(), value),
                    Span::styled(
                        match c.about.is_empty() {
                            true => String::new(),
                            false => format!("  {}", c.about),
                        },
                        Style::default().fg(Color::DarkGray),
                    ),
                ])
            })
            .collect();
        f.render_widget(Clear, area);
        f.render_widget(Paragraph::new(lines), area);
    }
}

/// The token the cursor is in, when it is one this can complete.
///
/// A slash only opens the list at the start of the line: a slash inside a sentence is a
/// path separator or a date, and a list that opens on those is a list in the way.
pub fn token_at(line: &str) -> Option<(char, usize, String)> {
    if let Some(rest) = line.strip_prefix('/') {
        // Only while the command itself is being typed; once there is a space the
        // argument belongs to whatever the command wants.
        if !rest.contains(char::is_whitespace) {
            return Some(('/', 0, rest.to_string()));
        }
        return None;
    }
    let at = line.rfind('@')?;
    let before = line[..at].chars().next_back();
    if before.is_some_and(|c| !c.is_whitespace()) {
        return None;
    }
    let token = &line[at + 1..];
    (!token.contains(char::is_whitespace)).then(|| ('@', at, token.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picker() -> Picker {
        Picker::new(
            '/',
            vec![
                Choice::new("model", "switch model"),
                Choice::new("mcp", "tool servers"),
                Choice::new("compact", "elide the oldest steps"),
            ],
        )
    }

    /// Somebody typing `mo` means `model` before `compact`, even though both contain the
    /// letters.
    #[test]
    fn what_starts_with_the_filter_comes_first() {
        let mut p = picker();
        p.narrow("m");
        let shown: Vec<&str> = p.shown().iter().map(|c| c.value.as_str()).collect();
        assert_eq!(shown, vec!["model", "mcp", "compact"]);
    }

    #[test]
    fn the_highlight_wraps_and_stays_inside_what_is_left() {
        let mut p = picker();
        p.narrow("mo");
        assert_eq!(p.shown().len(), 1);
        p.move_by(1);
        assert_eq!(p.chosen().as_deref(), Some("model"));
        p.move_by(-1);
        assert_eq!(p.chosen().as_deref(), Some("model"));
    }

    #[test]
    fn a_filter_that_leaves_nothing_offers_nothing() {
        let mut p = picker();
        p.narrow("zzz");
        assert!(p.is_empty());
        assert_eq!(p.chosen(), None);
    }

    /// A slash inside a sentence is a path separator or a date, and a list that opens on
    /// those is a list in the way.
    #[test]
    fn a_slash_opens_the_list_only_at_the_start() {
        assert_eq!(token_at("/mod").unwrap().0, '/');
        assert_eq!(token_at("/mod").unwrap().2, "mod");
        assert!(token_at("see src/main.rs").is_none());
        assert!(token_at("/model qwen").is_none());
    }

    #[test]
    fn an_at_opens_it_on_a_word_of_its_own() {
        let (sigil, at, token) = token_at("why does @src/ma").unwrap();
        assert_eq!(sigil, '@');
        assert_eq!(at, 9);
        assert_eq!(token, "src/ma");
        // Part of a longer word - an address, say - is not a file being named.
        assert!(token_at("write to a@b.com").is_none());
    }
}
