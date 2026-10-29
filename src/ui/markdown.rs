//! The markdown a model writes, as rows for a pane of a given width.
//!
//! Paragraphs, headings, lists, quotes, code blocks, tables, emphasis, links and rules.
//! Each block is wrapped here, so a list item or a quote keeps its indent on every row
//! it takes. A width of zero leaves lines unwrapped.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::ui::chat::{line_width, wrap};
use crate::ui::theme::theme;

/// The narrowest a block is wrapped to, however deep it is nested.
const NARROWEST: usize = 16;

/// Rows for `md` in a pane `width` columns wide.
pub fn render(md: &str, width: usize) -> Vec<Line<'static>> {
    let mut r = Renderer::new(width);
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for event in Parser::new_ext(md, options) {
        r.event(event);
    }
    r.finish()
}

/// One list being drawn: the number of its next item, or none for bullets.
struct List {
    next: Option<u64>,
}

/// A table being collected, drawn once it is complete.
#[derive(Default)]
struct Table {
    rows: Vec<Vec<Vec<Span<'static>>>>,
    header_rows: usize,
}

struct Renderer {
    width: usize,
    out: Vec<Line<'static>>,
    /// The spans of the block being written.
    spans: Vec<Span<'static>>,
    /// Emphasis, links and headings in force, innermost last.
    styles: Vec<Style>,
    lists: Vec<List>,
    /// The marker the next row of the current item starts with, until it has been used.
    marker: Option<String>,
    quotes: usize,
    /// The language and text of the code block being collected.
    code: Option<(String, String)>,
    table: Option<Table>,
    /// The link being written, for its address once its text is done.
    link: Option<String>,
    /// Whether the last thing written was a blank row.
    spaced: bool,
}

impl Renderer {
    fn new(width: usize) -> Self {
        Self {
            width,
            out: Vec::new(),
            spans: Vec::new(),
            styles: Vec::new(),
            lists: Vec::new(),
            marker: None,
            quotes: 0,
            code: None,
            table: None,
            link: None,
            spaced: true,
        }
    }

    fn style(&self) -> Style {
        self.styles
            .iter()
            .fold(Style::default(), |acc, s| acc.patch(*s))
    }

    /// What every row of the current block starts with: the quote bars and the indent of
    /// the lists it is in.
    fn gutter(&self) -> Vec<Span<'static>> {
        let mut g = Vec::new();
        for _ in 0..self.quotes {
            g.push(Span::styled("▎ ", theme().dim));
        }
        let depth = self.lists.len().saturating_sub(1);
        if depth > 0 {
            g.push(Span::raw("  ".repeat(depth)));
        }
        g
    }

    fn blank(&mut self) {
        if !self.spaced {
            self.out.push(Line::from(""));
            self.spaced = true;
        }
    }

    /// Write the spans gathered so far as one block, wrapped under its gutter.
    fn flush(&mut self) {
        if self.spans.is_empty() && self.marker.is_none() {
            return;
        }
        let spans = std::mem::take(&mut self.spans);
        let gutter = self.gutter();
        let marker = self.marker.take();
        let hang =
            marker
                .as_ref()
                .map(|m| m.chars().count())
                .unwrap_or(match self.lists.is_empty() {
                    true => 0,
                    false => 2,
                });
        let lead: usize = gutter
            .iter()
            .map(|s| s.content.chars().count())
            .sum::<usize>()
            + hang;
        let room = match self.width {
            0 => 0,
            w => w.saturating_sub(lead).max(NARROWEST),
        };
        for (i, row) in wrap(vec![Line::from(spans)], room).into_iter().enumerate() {
            let mut line = gutter.clone();
            match (i, &marker) {
                (0, Some(m)) => {
                    line.push(Span::styled(m.clone(), Style::default().fg(theme().accent)))
                }
                _ => line.push(Span::raw(" ".repeat(hang))),
            }
            line.extend(row.spans);
            self.out.push(Line::from(line));
        }
        self.spaced = false;
    }

    fn text(&mut self, text: &str) {
        if let Some((_, code)) = self.code.as_mut() {
            code.push_str(text);
            return;
        }
        let style = self.style();
        self.spans.push(Span::styled(text.to_string(), style));
    }

    fn event(&mut self, event: Event<'_>) {
        if let Some(table) = self.table.as_mut() {
            match event {
                Event::Start(Tag::TableRow) | Event::Start(Tag::TableHead) => {
                    table.rows.push(Vec::new())
                }
                Event::End(TagEnd::TableHead) => table.header_rows = table.rows.len(),
                Event::Start(Tag::TableCell) => {
                    if let Some(row) = table.rows.last_mut() {
                        row.push(Vec::new());
                    }
                }
                Event::Text(t) => {
                    let style = self
                        .styles
                        .iter()
                        .fold(Style::default(), |a, s| a.patch(*s));
                    if let Some(cell) = table.rows.last_mut().and_then(|r| r.last_mut()) {
                        cell.push(Span::styled(t.to_string(), style));
                    }
                }
                Event::Code(t) => {
                    if let Some(cell) = table.rows.last_mut().and_then(|r| r.last_mut()) {
                        cell.push(Span::styled(t.to_string(), theme().inline_code));
                    }
                }
                Event::Html(t) | Event::InlineHtml(t) => {
                    if let Some(cell) = table.rows.last_mut().and_then(|r| r.last_mut()) {
                        cell.push(Span::styled(t.to_string(), theme().dim));
                    }
                }
                Event::Start(Tag::Strong) => self.styles.push(bold()),
                Event::Start(Tag::Emphasis) => self.styles.push(italic()),
                Event::End(TagEnd::Strong) | Event::End(TagEnd::Emphasis) => {
                    self.styles.pop();
                }
                Event::End(TagEnd::Table) => {
                    let table = self.table.take().unwrap_or_default();
                    self.draw_table(table);
                }
                _ => {}
            }
            return;
        }
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => self.text(&t),
            Event::Code(t) => self
                .spans
                .push(Span::styled(t.to_string(), theme().inline_code)),
            Event::Html(t) | Event::InlineHtml(t) => {
                let t = t.trim_end_matches('\n').to_string();
                self.spans.push(Span::styled(t, theme().dim));
            }
            Event::SoftBreak => self.text(" "),
            Event::HardBreak => self.flush(),
            Event::Rule => {
                self.flush();
                self.blank();
                self.out
                    .push(Line::from(Span::styled("  ·  ·  ·", theme().dim)));
                self.spaced = false;
                self.blank();
            }
            Event::TaskListMarker(done) => self.spans.push(Span::styled(
                match done {
                    true => "[x] ",
                    false => "[ ] ",
                },
                Style::default().fg(theme().accent),
            )),
            Event::FootnoteReference(t) => self.text(&format!("[{t}]")),
            Event::InlineMath(t) | Event::DisplayMath(t) => self.text(&t),
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => {
                self.flush();
                self.blank();
                self.styles.push(match level {
                    HeadingLevel::H1 => theme().heading.fg(theme().accent),
                    _ => theme().heading,
                });
            }
            Tag::BlockQuote(_) => {
                self.flush();
                self.blank();
                self.quotes += 1;
            }
            Tag::CodeBlock(kind) => {
                self.flush();
                self.blank();
                let lang = match kind {
                    CodeBlockKind::Fenced(l) => {
                        l.split(',').next().unwrap_or("").trim().to_string()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((lang, String::new()));
            }
            Tag::List(start) => {
                self.flush();
                if self.lists.is_empty() {
                    self.blank();
                }
                self.lists.push(List { next: start });
            }
            Tag::Item => {
                self.flush();
                let marker = match self.lists.last_mut() {
                    Some(List { next: Some(n) }) => {
                        let m = format!("{n}. ");
                        *n += 1;
                        m
                    }
                    _ => "• ".to_string(),
                };
                self.marker = Some(marker);
            }
            Tag::Emphasis => self.styles.push(italic()),
            Tag::Strong => self.styles.push(bold()),
            Tag::Strikethrough => self
                .styles
                .push(Style::default().add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { dest_url, .. } => {
                self.styles.push(
                    Style::default()
                        .fg(theme().accent)
                        .add_modifier(Modifier::UNDERLINED),
                );
                self.link = Some(dest_url.to_string());
            }
            Tag::Image { dest_url, .. } => {
                self.text("[image ");
                self.link = Some(dest_url.to_string());
            }
            Tag::Table(_) => {
                self.flush();
                self.blank();
                self.table = Some(Table::default());
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush();
                // Items of a tight list sit on consecutive rows, so only a paragraph that
                // is not in a list is followed by a blank row.
                if self.lists.is_empty() {
                    self.blank();
                }
            }
            TagEnd::Heading(_) => {
                self.flush();
                self.styles.pop();
                self.blank();
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quotes = self.quotes.saturating_sub(1);
                self.blank();
            }
            TagEnd::CodeBlock => {
                let (lang, code) = self.code.take().unwrap_or_default();
                self.draw_code(&lang, &code);
                self.blank();
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
                if self.lists.is_empty() {
                    self.blank();
                }
            }
            TagEnd::Item => self.flush(),
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.styles.pop();
            }
            TagEnd::Link => {
                self.styles.pop();
                if let Some(url) = self.link.take() {
                    let said: String = self
                        .spans
                        .iter()
                        .rev()
                        .take(1)
                        .map(|s| s.content.to_string())
                        .collect();
                    if said != url {
                        self.spans
                            .push(Span::styled(format!(" ({url})"), theme().dim));
                    }
                }
            }
            TagEnd::Image => {
                if let Some(url) = self.link.take() {
                    self.text(&format!(" {url}]"));
                }
            }
            _ => {}
        }
    }

    /// A code block: its language on a row of its own, then its lines on the code surface,
    /// each padded to the width so the surface reads as one block.
    fn draw_code(&mut self, lang: &str, code: &str) {
        let gutter = self.gutter();
        let lead: usize = gutter
            .iter()
            .map(|s| s.content.chars().count())
            .sum::<usize>()
            + 2;
        let surface = match theme().tint {
            Some(tint) => Style::default().bg(tint),
            None => Style::default(),
        };
        // The language heads the block, on its surface, so the block reads as one thing.
        if !lang.is_empty() {
            let mut line = gutter.clone();
            line.push(Span::raw("  "));
            let label = Line::from(Span::styled(lang.to_string(), theme().dim.patch(surface)));
            let pad = match (self.width, theme().tint) {
                (0, _) | (_, None) => 0,
                (w, Some(_)) => w.saturating_sub(lead + line_width(&label)),
            };
            line.extend(label.spans);
            if pad > 0 {
                line.push(Span::styled(" ".repeat(pad), surface));
            }
            self.out.push(Line::from(line));
        }
        let code = code.trim_end_matches('\n');
        let base = theme().code.patch(surface);
        let rows = crate::ui::highlight::rows(lang, code, base).unwrap_or_else(|| {
            code.split('\n')
                .map(|raw| vec![Span::styled(raw.to_string(), base)])
                .collect()
        });
        for row in rows {
            let mut line = gutter.clone();
            line.push(Span::raw("  "));
            let body = Line::from(row);
            let pad = match (self.width, theme().tint) {
                (0, _) | (_, None) => 0,
                (w, Some(_)) => w.saturating_sub(lead + line_width(&body)),
            };
            line.extend(body.spans);
            if pad > 0 {
                line.push(Span::styled(" ".repeat(pad), surface));
            }
            self.out.push(Line::from(line));
        }
        self.spaced = false;
    }

    /// A table: columns as wide as their widest cell, the header bold and underlined, two
    /// spaces between columns, and no rules.
    fn draw_table(&mut self, table: Table) {
        let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        let wide = |cell: &Vec<Span<'static>>| -> usize {
            cell.iter().map(|s| s.content.chars().count()).sum()
        };
        // A row with only its first cell filled heads the rows under it, and takes no part
        // in how wide the first column is.
        let heads = |row: &Vec<Vec<Span<'static>>>| {
            row.len() > 1 && row.iter().skip(1).all(|c| wide(c) == 0)
        };
        let mut widths = vec![0usize; columns];
        for (r, row) in table.rows.iter().enumerate() {
            if r >= table.header_rows && heads(row) {
                continue;
            }
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(wide(cell));
            }
        }
        let gutter = self.gutter();
        let lead = gutter
            .iter()
            .map(|s| s.content.chars().count())
            .sum::<usize>()
            + 2;
        // A table wider than the pane gives its last column what is left, and that column
        // wraps under itself rather than running off the edge.
        if let (Some(last), true) = (columns.checked_sub(1), self.width > 0) {
            let others: usize = widths[..last].iter().map(|w| w + 2).sum();
            let room = self.width.saturating_sub(lead + others).max(NARROWEST);
            widths[last] = widths[last].min(room);
        }
        for (r, row) in table.rows.iter().enumerate() {
            let header = r < table.header_rows;
            let style_of = |span: &Span<'static>| match header {
                true => span.style.patch(theme().dim).add_modifier(Modifier::BOLD),
                false => span.style,
            };
            let mut line = gutter.clone();
            line.push(Span::raw("  "));
            let mut under: Vec<Line<'static>> = Vec::new();
            for (i, cell) in row.iter().enumerate() {
                let spans: Vec<Span<'static>> = cell
                    .iter()
                    .map(|s| Span::styled(s.content.to_string(), style_of(s)))
                    .collect();
                if i + 1 < row.len() {
                    line.extend(spans);
                    line.push(Span::raw(
                        " ".repeat(widths[i].saturating_sub(wide(cell)) + 2),
                    ));
                    continue;
                }
                let mut rows = wrap(vec![Line::from(spans)], widths[i]).into_iter();
                if let Some(first) = rows.next() {
                    line.extend(first.spans);
                }
                let indent = lead + widths[..i].iter().map(|w| w + 2).sum::<usize>();
                for more in rows {
                    let mut l = gutter.clone();
                    l.push(Span::raw(" ".repeat(indent.saturating_sub(lead) + 2)));
                    l.extend(more.spans);
                    under.push(Line::from(l));
                }
            }
            self.out.push(Line::from(line));
            self.out.extend(under);
        }
        self.spaced = false;
        self.blank();
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        if let Some((lang, code)) = self.code.take() {
            self.draw_code(&lang, &code);
        }
        if let Some(table) = self.table.take() {
            self.draw_table(table);
        }
        self.flush();
        while self.out.last().is_some_and(|l| line_width(l) == 0) {
            self.out.pop();
        }
        self.out
    }
}

fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

fn italic() -> Style {
    Style::default().add_modifier(Modifier::ITALIC)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::chat::line_text;

    fn text(md: &str, width: usize) -> Vec<String> {
        render(md, width).iter().map(line_text).collect()
    }

    /// Markers are consumed, and every other character comes out in the order it went in.
    #[test]
    fn styling_consumes_the_markers_and_nothing_else() {
        for (body, expected) in [
            ("plain sentence", "plain sentence"),
            ("with `code` in it", "with code in it"),
            ("with **bold** in it", "with bold in it"),
            ("with *some* emphasis", "with some emphasis"),
            ("`code` and **bold** together", "code and bold together"),
        ] {
            assert_eq!(text(body, 0), vec![expected.to_string()], "{body:?}");
        }
    }

    /// A marker with nothing closing it is a literal character.
    #[test]
    fn an_unclosed_marker_is_left_as_written() {
        for body in ["a `mismatched backtick", "**unclosed bold", "50% of 100**"] {
            assert_eq!(text(body, 0), vec![body.to_string()], "{body:?}");
        }
    }

    #[test]
    fn inline_code_and_bold_are_marked_up() {
        let rows = render("run `cargo test` **now**", 0);
        let code: Vec<_> = rows[0]
            .spans
            .iter()
            .filter(|s| s.style == theme().inline_code)
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(code, vec!["cargo test"]);
        assert!(rows[0]
            .spans
            .iter()
            .any(|s| s.content == "now" && s.style.add_modifier.contains(Modifier::BOLD)));
    }

    /// Inside a fence the text is code: a `**` in a shell glob is not emphasis, and the
    /// fence itself gives way to the language on a row of its own.
    #[test]
    fn a_fenced_block_is_left_alone() {
        let rows = text("before\n\n```sh\nls **/*.rs\n```\n\nafter", 0);
        assert_eq!(
            rows,
            vec!["before", "", "  sh", "  ls **/*.rs", "", "after"]
        );
    }

    /// A list item wraps under its own text, not under its bullet.
    #[test]
    fn a_list_item_keeps_its_indent_when_it_wraps() {
        let rows = text("- one two three four five six\n- seven", 20);
        assert_eq!(rows, vec!["• one two three four", "  five six", "• seven"]);
        let rows = text("1. first\n2. second", 0);
        assert_eq!(rows, vec!["1. first", "2. second"]);
    }

    /// A nested list sits further in than the one it belongs to.
    #[test]
    fn a_nested_list_is_indented() {
        let rows = text("- outer\n  - inner", 0);
        assert_eq!(rows, vec!["• outer", "  • inner"]);
    }

    /// A quote carries its bar on every row.
    #[test]
    fn a_quote_has_a_bar_on_every_row() {
        let rows = text("> one two three four", 12);
        assert!(rows.len() > 1);
        assert!(rows.iter().all(|r| r.starts_with("▎ ")), "{rows:?}");
    }

    /// A table lines its columns up without drawing any rule.
    #[test]
    fn a_table_lines_up_its_columns() {
        let rows = text("| a | bb |\n|---|---|\n| ccc | d |", 0);
        assert_eq!(rows, vec!["  a    bb", "  ccc  d"]);
    }

    /// A table wider than the pane wraps its last column under itself, and a word in angle
    /// brackets in a cell is kept.
    #[test]
    fn a_wide_table_wraps_its_last_column() {
        let rows = text(
            "| Key | What |\n|---|---|\n| `/model <name>` | switch to another model and remember it |",
            34,
        );
        assert_eq!(
            rows,
            vec![
                "  Key            What",
                "  /model <name>  switch to another",
                "                 model and",
                "                 remember it",
            ]
        );
    }

    /// A row with only its first cell filled heads the rows under it without widening the
    /// first column.
    #[test]
    fn a_heading_row_does_not_widen_its_column() {
        let rows = text(
            "| A | B |\n|---|---|\n| **a long heading** | |\n| x | y |",
            0,
        );
        assert_eq!(rows[2], "  x  y");
    }

    /// A link shows where it goes, unless its text already says so.
    #[test]
    fn a_link_shows_its_address() {
        assert_eq!(
            text("[docs](https://x.org)", 0),
            vec!["docs (https://x.org)"]
        );
        assert_eq!(text("<https://x.org>", 0), vec!["https://x.org"]);
    }

    /// An answer still being written, with a fence not yet closed, is drawn as code.
    #[test]
    fn an_open_fence_is_code_while_it_streams() {
        let rows = text("look:\n\n```rust\nfn main() {", 0);
        assert_eq!(rows, vec!["look:", "", "  rust", "  fn main() {"]);
    }
}
