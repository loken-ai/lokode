//! A unified diff as rows: each line under its number, added and removed lines on a
//! surface of their own, the words that changed picked out, and the code coloured by
//! its language when the file names one.
//!
//! The file headers give way to the path, once per file. A pair of lines that changed
//! only in part has the changed words marked. A pair that changed throughout is shown
//! as a removal and an addition, with nothing marked.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use similar::{ChangeTag, TextDiff};

use crate::ui::chat::mark_range;
use crate::ui::theme::theme;

/// How alike a removed and an added line have to be for the words between them to be
/// marked, as the share of their words they keep.
const ALIKE: f32 = 0.5;

/// Runs of columns, each from its first column up to but not including its last.
type Marks = Vec<(usize, usize)>;

/// What one line of a diff is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Context,
    Added,
    Removed,
}

/// One line of a hunk, with the number it has on the side it belongs to.
struct Change<'a> {
    kind: Kind,
    number: usize,
    text: &'a str,
    /// Columns of `text` that changed against the line it pairs with.
    marked: Marks,
}

/// A row before it is drawn.
enum Row<'a> {
    Path(String),
    Hunk(&'a str),
    Change(Change<'a>),
    Other(&'a str),
}

/// The rows of the unified diff `text` for a pane `width` columns wide. Zero leaves the
/// rows unpadded.
pub fn rows(text: &str, width: usize) -> Vec<Line<'static>> {
    let parsed = parse(text);
    let widest = parsed
        .iter()
        .filter_map(|r| match r {
            Row::Change(c) => Some(c.number),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    let digits = widest.to_string().len().max(3);
    let mut lang: Option<String> = None;
    let mut out = Vec::with_capacity(parsed.len());
    for row in parsed {
        out.push(match row {
            Row::Path(path) => {
                lang = crate::ui::highlight::language_for(&path);
                Line::from(Span::styled(path, theme().dim))
            }
            Row::Hunk(h) => Line::from(Span::styled(
                h.to_string(),
                Style::default().fg(theme().hunk),
            )),
            Row::Other(o) => Line::from(Span::styled(o.to_string(), theme().dim)),
            Row::Change(c) => change_row(&c, digits, lang.as_deref(), width),
        });
    }
    out
}

fn change_row(c: &Change<'_>, digits: usize, lang: Option<&str>, width: usize) -> Line<'static> {
    let t = theme();
    let (sign, line, word) = match c.kind {
        Kind::Context => (' ', Style::default(), Style::default()),
        Kind::Added => ('+', t.diff.added, t.diff.added_word),
        Kind::Removed => ('-', t.diff.removed, t.diff.removed_word),
    };
    let sign_style = match c.kind {
        Kind::Context => t.dim,
        Kind::Added => line.fg(t.good),
        Kind::Removed => line.fg(t.bad),
    };
    // On a tinted surface the code keeps its colours. Without one, the colour of an added
    // or removed line is all that says which it is, so it stays.
    let coloured = match (c.kind, t.diff.tinted) {
        (Kind::Context, _) | (_, true) => {
            lang.and_then(|l| crate::ui::highlight::rows(l, c.text, line))
        }
        _ => None,
    };
    let code: Vec<Span<'static>> = coloured
        .and_then(|rows| rows.into_iter().next())
        .unwrap_or_else(|| vec![Span::styled(c.text.to_string(), line)]);
    let lead = vec![
        Span::styled(format!("{:>digits$} ", c.number), t.dim.patch(line)),
        Span::styled(format!("{sign} "), sign_style),
    ];
    let lead_width = digits + 3;
    let mut row = lead;
    row.extend(code);
    let mut row = Line::from(row);
    for &(from, to) in &c.marked {
        row = mark_range(&row, lead_width + from, lead_width + to, word);
    }
    let used = crate::ui::chat::line_width(&row);
    if width > used && line.bg.is_some() {
        row.spans.push(Span::styled(" ".repeat(width - used), line));
    }
    row
}

fn parse(text: &str) -> Vec<Row<'_>> {
    let mut out = Vec::new();
    let (mut old, mut new) = (0usize, 0usize);
    let mut in_hunk = false;
    for l in text.lines() {
        if let Some(path) = l.strip_prefix("+++ ") {
            let path = path.split('\t').next().unwrap_or(path);
            let path = path.strip_prefix("b/").unwrap_or(path);
            out.push(Row::Path(path.to_string()));
            in_hunk = false;
        } else if l.starts_with("--- ") || l.starts_with("diff --git ") || l.starts_with("index ") {
            in_hunk = false;
        } else if let Some((o, n)) = hunk_start(l) {
            (old, new) = (o, n);
            in_hunk = true;
            out.push(Row::Hunk(l));
        } else if in_hunk && l.starts_with('+') {
            out.push(Row::Change(Change {
                kind: Kind::Added,
                number: new,
                text: &l[1..],
                marked: Vec::new(),
            }));
            new += 1;
        } else if in_hunk && l.starts_with('-') {
            out.push(Row::Change(Change {
                kind: Kind::Removed,
                number: old,
                text: &l[1..],
                marked: Vec::new(),
            }));
            old += 1;
        } else if in_hunk && (l.starts_with(' ') || l.is_empty()) {
            out.push(Row::Change(Change {
                kind: Kind::Context,
                number: new,
                text: l.get(1..).unwrap_or(""),
                marked: Vec::new(),
            }));
            old += 1;
            new += 1;
        } else {
            out.push(Row::Other(l));
        }
    }
    mark_pairs(&mut out);
    out
}

/// The first line numbers of a hunk, from its header: `@@ -12,3 +12,4 @@`.
fn hunk_start(l: &str) -> Option<(usize, usize)> {
    let rest = l.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let new = rest.split_once(" @@")?.0;
    let first = |s: &str| s.split(',').next()?.parse::<usize>().ok();
    Some((first(old)?, first(new)?))
}

/// Mark the words that changed between each removed line and the added line in the same
/// place of the run that follows it.
fn mark_pairs(rows: &mut [Row<'_>]) {
    let kind = |r: &Row<'_>| match r {
        Row::Change(c) => Some(c.kind),
        _ => None,
    };
    let mut i = 0;
    while i < rows.len() {
        if kind(&rows[i]) != Some(Kind::Removed) {
            i += 1;
            continue;
        }
        let removed_from = i;
        while i < rows.len() && kind(&rows[i]) == Some(Kind::Removed) {
            i += 1;
        }
        let added_from = i;
        while i < rows.len() && kind(&rows[i]) == Some(Kind::Added) {
            i += 1;
        }
        let pairs = (added_from - removed_from).min(i - added_from);
        for k in 0..pairs {
            let (old, new) = match (&rows[removed_from + k], &rows[added_from + k]) {
                (Row::Change(a), Row::Change(b)) => (a.text, b.text),
                _ => continue,
            };
            let (old_marks, new_marks) = changed_words(old, new);
            if let Row::Change(c) = &mut rows[removed_from + k] {
                c.marked = old_marks;
            }
            if let Row::Change(c) = &mut rows[added_from + k] {
                c.marked = new_marks;
            }
        }
    }
}

/// The columns of `old` and of `new` that differ, word by word, or nothing when the two
/// lines have too little in common for that to help.
fn changed_words(old: &str, new: &str) -> (Marks, Marks) {
    let diff = TextDiff::from_words(old, new);
    if diff.ratio() < ALIKE {
        return (Vec::new(), Vec::new());
    }
    let (mut old_marks, mut new_marks) = (Vec::new(), Vec::new());
    let (mut at_old, mut at_new) = (0usize, 0usize);
    for change in diff.iter_all_changes() {
        let wide = change.value().chars().count();
        match change.tag() {
            ChangeTag::Equal => {
                at_old += wide;
                at_new += wide;
            }
            ChangeTag::Delete => {
                push_mark(&mut old_marks, at_old, at_old + wide, change.value());
                at_old += wide;
            }
            ChangeTag::Insert => {
                push_mark(&mut new_marks, at_new, at_new + wide, change.value());
                at_new += wide;
            }
        }
    }
    (
        join_across_spaces(old_marks, old),
        join_across_spaces(new_marks, new),
    )
}

/// Marks that only spaces separate, as one mark: the words of one change read as one.
fn join_across_spaces(marks: Marks, text: &str) -> Marks {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Marks = Vec::with_capacity(marks.len());
    for (from, to) in marks {
        match out.last_mut() {
            Some(last) if chars[last.1..from].iter().all(|c| c.is_whitespace()) => last.1 = to,
            _ => out.push((from, to)),
        }
    }
    out
}

/// Add a marked run, joined to the one before when they touch, and never over spaces
/// alone.
fn push_mark(marks: &mut Marks, from: usize, to: usize, value: &str) {
    if value.trim().is_empty() {
        return;
    }
    match marks.last_mut() {
        Some(last) if last.1 == from => last.1 = to,
        _ => marks.push((from, to)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::chat::line_text;

    const EDIT: &str = "--- a/src/mcp/client.rs\n+++ b/src/mcp/client.rs\n@@ -211,3 +211,3 @@ impl Client\n     let (tx, rx) = oneshot::channel();\n-    let reply = rx.await?;\n+    let reply = timeout(self.timeout, rx).await?;\n     Ok(reply)";

    fn text(diff: &str) -> Vec<String> {
        rows(diff, 0).iter().map(line_text).collect()
    }

    /// Each line sits under the number it has on its side, and the file headers give way
    /// to the path.
    #[test]
    fn lines_are_numbered_on_their_own_side() {
        assert_eq!(
            text(EDIT),
            vec![
                "src/mcp/client.rs",
                "@@ -211,3 +211,3 @@ impl Client",
                "211       let (tx, rx) = oneshot::channel();",
                "212 -     let reply = rx.await?;",
                "212 +     let reply = timeout(self.timeout, rx).await?;",
                "213       Ok(reply)",
            ]
        );
    }

    /// Only the words that changed are marked, on both lines of the pair.
    #[test]
    fn the_changed_words_are_marked() {
        let (old, new) = changed_words(
            "    let reply = rx.await?;",
            "    let reply = timeout(self.timeout, rx).await?;",
        );
        let cut = |s: &str, m: &[(usize, usize)]| -> Vec<String> {
            m.iter()
                .map(|&(a, b)| s.chars().skip(a).take(b - a).collect())
                .collect()
        };
        assert_eq!(cut("    let reply = rx.await?;", &old), vec!["rx.await?;"]);
        assert_eq!(
            cut("    let reply = timeout(self.timeout, rx).await?;", &new),
            vec!["timeout(self.timeout, rx).await?;"]
        );
    }

    /// Two lines with nothing in common are a removal and an addition, nothing marked.
    #[test]
    fn unrelated_lines_mark_nothing() {
        let (old, new) = changed_words("alpha beta gamma", "one two three four");
        assert!(old.is_empty() && new.is_empty());
    }

    /// The sign of a line carries its colour, whatever colours the code takes.
    #[test]
    fn added_and_removed_lines_say_which_they_are() {
        let rows = rows(EDIT, 0);
        let sign_fg = |row: usize| rows[row].spans[1].style.fg;
        assert_eq!(sign_fg(3), Some(theme().bad));
        assert_eq!(sign_fg(4), Some(theme().good));
    }

    /// A hunk header that does not parse is shown as it is, not lost.
    #[test]
    fn a_line_outside_a_hunk_is_kept() {
        assert_eq!(
            text("just a line\n+not in a hunk"),
            vec!["just a line", "+not in a hunk"]
        );
    }
}
