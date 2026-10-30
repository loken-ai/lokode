//! A list to choose from, drawn the same way wherever one opens.
//!
//! A raised card: a head naming what is chosen, how many there are and the keys.
//! Then the values in a column, with what each one is beside it.
//! The highlighted row runs across the whole card.
//! The letters typed are picked out in each value.
//! What is in force now is marked wherever it sits.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

use crate::ui::picker::Choice;
use crate::ui::theme::theme;

/// The widest the column of values gets, whatever the values are.
const VALUE_AT_MOST: usize = 36;

/// A list as it is about to be drawn.
pub struct Listing<'a> {
    /// What is being chosen.
    pub title: &'a str,
    /// What the filter leaves, best first.
    pub items: &'a [&'a Choice],
    /// The highlighted item.
    pub at: usize,
    /// The first item on screen, and how many rows there are for items.
    pub first: usize,
    pub rows: usize,
    /// What has been typed to narrow the list.
    pub filter: &'a str,
    /// The value in force now, marked wherever it is.
    pub marked: Option<&'a str>,
    /// The keys, as pairs of key and what it does.
    pub keys: &'a [(&'a str, &'a str)],
    /// Whether the values are paths, whose directories recede.
    pub paths: bool,
}

impl Listing<'_> {
    /// The card's rows, `width` columns wide: its head, then one row per item on screen.
    pub fn lines(&self, width: usize) -> Vec<Line<'static>> {
        let t = theme();
        let surface = match t.raised {
            Some(c) => Style::default().bg(c),
            None => Style::default(),
        };
        let mut out = vec![self.head(width, surface)];
        let shown = &self.items[self.first.min(self.items.len())..];
        let value_w = shown
            .iter()
            .take(self.rows)
            .map(|c| c.value.chars().count())
            .max()
            .unwrap_or(0)
            .min(VALUE_AT_MOST)
            .min(width / 2);
        for (i, choice) in shown.iter().take(self.rows).enumerate() {
            let here = self.first + i == self.at;
            let row = match here {
                true => t.chosen_row,
                false => surface,
            };
            let mark = match self.marked == Some(choice.value.as_str()) {
                true => Span::styled(" ● ", row.fg(t.accent)),
                false => Span::styled("   ", row),
            };
            let mut spans = vec![mark];
            spans.extend(self.value_spans(&choice.value, value_w, row, here));
            let used = 3 + value_w;
            if !choice.about.is_empty() && width > used + 4 {
                let room = width - used - 3;
                spans.push(Span::styled(
                    format!("  {}", clip(&choice.about, room)),
                    row.patch(t.dim)
                        .bg(row.bg.unwrap_or(ratatui::style::Color::Reset)),
                ));
            }
            out.push(pad(spans, width, row));
        }
        if self.items.is_empty() {
            out.push(pad(
                vec![Span::styled("   nothing matches", surface.patch(t.dim))],
                width,
                surface,
            ));
        }
        out
    }

    /// The head.
    /// On the left it holds what is chosen and what was typed.
    /// On the right it holds how many there are and the keys.
    fn head(&self, width: usize, surface: Style) -> Line<'static> {
        let t = theme();
        let mut left = vec![Span::styled(
            format!(" {} ", self.title),
            surface.fg(t.accent).add_modifier(Modifier::BOLD),
        )];
        if !self.filter.is_empty() {
            left.push(Span::styled("› ", surface.patch(t.dim)));
            left.push(Span::styled(self.filter.to_string(), surface.fg(t.warn)));
        }
        let count = match self.items.len() {
            0 => "0".to_string(),
            n => format!("{} of {n}", self.at + 1),
        };
        let mut right = vec![Span::styled(count, surface.patch(t.dim))];
        for (key, what) in self.keys {
            right.push(Span::styled(format!("   {key}"), surface.patch(t.key)));
            right.push(Span::styled(format!(" {what}"), surface.patch(t.dim)));
        }
        right.push(Span::styled(" ", surface));
        let wide = |s: &[Span<'static>]| s.iter().map(|x| x.content.chars().count()).sum::<usize>();
        let mut spans = left;
        let gap = width.saturating_sub(wide(&spans) + wide(&right));
        if gap >= 1 {
            spans.push(Span::styled(" ".repeat(gap), surface));
            spans.extend(right);
        }
        Line::from(spans)
    }

    /// A value padded to its column.
    /// The letters typed are picked out.
    /// A path's directory recedes behind its name.
    fn value_spans(&self, value: &str, width: usize, row: Style, here: bool) -> Vec<Span<'static>> {
        let t = theme();
        let shown = clip(value, width);
        let chars: Vec<char> = shown.chars().collect();
        let name_from = match self.paths {
            true => shown
                .trim_end_matches('/')
                .rfind('/')
                .map(|i| shown[..=i].chars().count())
                .unwrap_or(0),
            false => 0,
        };
        let hit = (!self.filter.is_empty())
            .then(|| find_ci(&shown, self.filter))
            .flatten();
        let base = match here {
            true => row.add_modifier(Modifier::BOLD),
            false => row,
        };
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (i, c) in chars.iter().enumerate() {
            let mut style = match i < name_from {
                true => base
                    .patch(t.dim)
                    .bg(row.bg.unwrap_or(ratatui::style::Color::Reset)),
                false => base,
            };
            if let Some((from, to)) = hit {
                if i >= from && i < to {
                    style = style.fg(t.accent).add_modifier(Modifier::BOLD);
                }
            }
            match spans.last_mut() {
                Some(last) if last.style == style => last.content.to_mut().push(*c),
                _ => spans.push(Span::styled(c.to_string(), style)),
            }
        }
        let pad_to = width.saturating_sub(chars.len());
        if pad_to > 0 {
            spans.push(Span::styled(" ".repeat(pad_to), row));
        }
        spans
    }

    /// Draw the card into `area`.
    pub fn render(&self, f: &mut Frame, area: Rect) {
        let surface = match theme().raised {
            Some(c) => Style::default().bg(c),
            None => Style::default(),
        };
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(self.lines(area.width as usize)).style(surface),
            area,
        );
    }
}

/// `spans` filled out to `width` in `style`, so a row's colour runs across the card.
fn pad(mut spans: Vec<Span<'static>>, width: usize, style: Style) -> Line<'static> {
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    if width > used {
        spans.push(Span::styled(" ".repeat(width - used), style));
    }
    Line::from(spans)
}

/// Where `needle` first sits in `hay`, in characters, ignoring case.
fn find_ci(hay: &str, needle: &str) -> Option<(usize, usize)> {
    let hay: Vec<char> = hay.to_lowercase().chars().collect();
    let needle: Vec<char> = needle.to_lowercase().chars().collect();
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    (0..=hay.len() - needle.len())
        .find(|&i| hay[i..i + needle.len()] == needle[..])
        .map(|i| (i, i + needle.len()))
}

/// `s` cut to `n` characters, ending in an ellipsis when it was cut.
fn clip(s: &str, n: usize) -> String {
    match s.chars().count() > n {
        true => s.chars().take(n.saturating_sub(1)).chain(['…']).collect(),
        false => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::chat::line_text;

    fn choices() -> Vec<Choice> {
        vec![
            Choice::new("model", "what the backend serves"),
            Choice::new(
                "compact",
                "elide the oldest steps now rather than when the window fills",
            ),
        ]
    }

    /// The head says what is chosen, where the highlight is and the keys.
    /// The values sit in one column.
    /// A long description is cut to the card.
    #[test]
    fn a_list_is_a_head_and_a_column() {
        let all = choices();
        let items: Vec<&Choice> = all.iter().collect();
        let listing = Listing {
            title: "commands",
            items: &items,
            at: 1,
            first: 0,
            rows: 8,
            filter: "",
            marked: Some("model"),
            keys: &[("tab", "take")],
            paths: false,
        };
        let rows: Vec<String> = listing.lines(60).iter().map(line_text).collect();
        assert!(rows[0].starts_with(" commands "), "{rows:?}");
        assert!(
            rows[0].trim_end().ends_with("2 of 2   tab take"),
            "{rows:?}"
        );
        assert!(
            rows[1].starts_with(" ● model    what the backend serves"),
            "{rows:?}"
        );
        assert!(rows[2].starts_with("   compact  elide"), "{rows:?}");
        assert!(rows[2].trim_end().ends_with('…'), "{rows:?}");
        assert!(rows.iter().all(|r| r.chars().count() == 60), "{rows:?}");
    }

    /// What was typed is found in a value whatever its case.
    #[test]
    fn the_letters_typed_are_found() {
        assert_eq!(find_ci("src/ui/Chat.rs", "chat"), Some((7, 11)));
        assert_eq!(find_ci("model", "x"), None);
    }
}
