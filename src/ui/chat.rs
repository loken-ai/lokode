use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// A single rendered entry in the chat scrollback.
#[derive(Debug, Clone)]
pub enum Entry {
    User(String),
    Assistant(String),
    AssistantStreaming(String),
    ToolCall {
        name: String,
        args: String,
    },
    ToolResult {
        name: String,
        is_error: bool,
        body: String,
    },
    /// What a tool is printing while it runs, kept as one growing entry.
    ToolOutput {
        name: String,
        body: String,
    },
    Notice(String),
    /// The model's scratchpad, accumulated while it streams.
    ///
    /// Shown dimmed and set apart, because it is the one thing in the pane the user must
    /// not mistake for the answer.
    Reasoning(String),
}

impl Entry {
    pub fn to_lines(&self) -> Vec<Line<'static>> {
        match self {
            Entry::User(s) => block("you", Color::Cyan, s, Body::Plain),
            Entry::Assistant(s) | Entry::AssistantStreaming(s) => {
                block("assistant", Color::Green, s, Body::Prose)
            }
            Entry::ToolCall { name, args } => block(
                &format!("→ {name}"),
                Color::Yellow,
                &truncate(args, 800),
                Body::Plain,
            ),
            Entry::ToolResult {
                name,
                is_error,
                body,
            } => {
                let label = if *is_error {
                    format!("← {name} [error]")
                } else {
                    format!("← {name}")
                };
                block(
                    &label,
                    if *is_error {
                        Color::Red
                    } else {
                        Color::Magenta
                    },
                    &truncate(body, 1600),
                    // A tool that returns a diff - a write, an edit, a patch - is the case
                    // where colour carries the meaning rather than decorating it.
                    if looks_like_diff(body) {
                        Body::Diff
                    } else {
                        Body::Plain
                    },
                )
            }
            // `/diff` answers through a notice, and a wall of uncoloured diff is the thing
            // it is least useful as.
            Entry::Reasoning(s) => {
                let mut lines = block("thinking", Color::DarkGray, s, Body::Plain);
                // Dim the body as well as the label: the header alone is not enough to
                // stop a wall of scratchpad reading as the answer.
                for line in lines.iter_mut().skip(1) {
                    for span in line.spans.iter_mut() {
                        if span.style.fg.is_none() {
                            span.style = span.style.fg(Color::DarkGray);
                        }
                    }
                }
                lines
            }
            // Dimmed and set apart: it is what is happening, not what was said, and it
            // is replaced by the tool's own result the moment there is one.
            Entry::ToolOutput { name, body } => {
                block(&format!("… {name}"), Color::DarkGray, body, Body::Plain)
            }
            Entry::Notice(s) => block(
                "·",
                Color::DarkGray,
                s,
                if looks_like_diff(s) {
                    Body::Diff
                } else {
                    Body::Plain
                },
            ),
        }
    }
}

/// How a body should be read.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Body {
    Plain,
    /// Light markdown: what a model actually emits, and nothing more.
    Prose,
    Diff,
}

/// Whether a body is a unified diff.
///
/// Judged on the header a diff has and prose does not, rather than on leading `+` and `-`,
/// which a bulleted list also has.
fn looks_like_diff(s: &str) -> bool {
    s.lines()
        .any(|l| l.starts_with("@@ ") || l.starts_with("--- ") || l.starts_with("diff --git "))
}

fn block(label: &str, color: Color, body: &str, kind: Body) -> Vec<Line<'static>> {
    let header = Line::from(Span::styled(
        format!("[{label}]"),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    ));
    let mut out = vec![header];
    match kind {
        Body::Plain => out.extend(body.lines().map(|l| Line::from(l.to_string()))),
        Body::Diff => out.extend(body.lines().map(diff_line)),
        Body::Prose => out.extend(prose(body)),
    }
    out.push(Line::from(""));
    out
}

fn diff_line(l: &str) -> Line<'static> {
    let style = if l.starts_with("+++") || l.starts_with("---") {
        Style::default().fg(Color::DarkGray)
    } else if l.starts_with('+') {
        Style::default().fg(Color::Green)
    } else if l.starts_with('-') {
        Style::default().fg(Color::Red)
    } else if l.starts_with("@@") {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };
    Line::from(Span::styled(l.to_string(), style))
}

/// Render the markdown a model actually writes.
///
/// Fenced code, headings, and inline `code` and **bold** - no more than that. A full parser
/// would be a dependency and a syntax highlighter would be a large one, for a pane that
/// shows a few lines of prose between tool calls. What this does is make a code block
/// distinguishable from the sentence introducing it, which is the whole of the problem.
fn prose(body: &str) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut in_code = false;
    for raw in body.lines() {
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") {
            in_code = !in_code;
            out.push(Line::from(Span::styled(
                raw.to_string(),
                Style::default().fg(Color::DarkGray),
            )));
            continue;
        }
        // Inside a fence the text is code, so `**` is a glob and not emphasis.
        if in_code {
            out.push(Line::from(Span::styled(
                raw.to_string(),
                Style::default().fg(Color::Rgb(180, 200, 220)),
            )));
            continue;
        }
        if trimmed.starts_with("# ")
            || trimmed.starts_with("## ")
            || trimmed.starts_with("### ")
            || trimmed.starts_with("#### ")
        {
            out.push(Line::from(Span::styled(
                raw.to_string(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )));
            continue;
        }
        out.push(Line::from(inline(raw)));
    }
    out
}

/// Style the spans inside one line: `code` and **bold**, left as written otherwise.
fn inline(line: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = line;

    while !rest.is_empty() {
        let backtick = rest.find('`');
        let bold = rest.find("**");
        // Whichever marker comes first, if either does.
        let (at, marker, style) = match (backtick, bold) {
            (Some(b), Some(s)) if b <= s => (b, "`", Style::default().fg(Color::Yellow)),
            (Some(b), None) => (b, "`", Style::default().fg(Color::Yellow)),
            (_, Some(s)) => (s, "**", Style::default().add_modifier(Modifier::BOLD)),
            (None, None) => break,
        };

        let (before, after) = rest.split_at(at);
        if !before.is_empty() {
            spans.push(Span::raw(before.to_string()));
        }
        let after = &after[marker.len()..];
        match after.find(marker) {
            Some(end) => {
                spans.push(Span::styled(after[..end].to_string(), style));
                rest = &after[end + marker.len()..];
            }
            // An opening marker with no close is not emphasis, it is a backtick.
            None => {
                spans.push(Span::raw(format!("{marker}{after}")));
                rest = "";
            }
        }
    }
    if !rest.is_empty() {
        spans.push(Span::raw(rest.to_string()));
    }
    if spans.is_empty() {
        spans.push(Span::raw(line.to_string()));
    }
    spans
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut t = s.chars().take(n).collect::<String>();
    t.push_str("\n… (truncated)");
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    /// Markers are consumed - that is what rendering them means - but nothing else may be.
    /// Every other character has to come out in the order it went in.
    #[test]
    fn styling_consumes_the_markers_and_nothing_else() {
        for (body, expected) in [
            ("plain sentence", "plain sentence"),
            ("with `code` in it", "with code in it"),
            ("with **bold** in it", "with bold in it"),
            ("`code` and **bold** together", "code and bold together"),
            ("back-to-back ``", "back-to-back "),
        ] {
            assert_eq!(text(&prose(body)), vec![expected.to_string()], "{body:?}");
        }
    }

    /// A marker with nothing closing it is a literal character, not the start of styling
    /// that swallows the rest of the line.
    #[test]
    fn an_unclosed_marker_is_left_as_written() {
        for body in ["a `mismatched backtick", "**unclosed bold", "50% of 100**"] {
            assert_eq!(text(&prose(body)), vec![body.to_string()], "{body:?}");
        }
    }

    #[test]
    fn inline_code_is_marked_up() {
        let spans = inline("run `cargo test` now");
        let styled: Vec<_> = spans
            .iter()
            .filter(|s| s.style.fg == Some(Color::Yellow))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(styled, vec!["cargo test"]);
    }

    #[test]
    fn bold_is_marked_up() {
        let spans = inline("this is **important** yes");
        let styled: Vec<_> = spans
            .iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::BOLD))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(styled, vec!["important"]);
    }

    /// A fenced block is where the model puts code, and it must not be read as prose - a
    /// `**` in a shell glob is not emphasis.
    #[test]
    fn a_fenced_block_is_left_alone() {
        let body = "before\n```sh\nls **/*.rs\n```\nafter";
        let rendered = text(&prose(body));
        assert_eq!(rendered[2], "ls **/*.rs");
    }

    /// Judged on a diff's header, because a bulleted list also starts lines with `-`.
    #[test]
    fn a_diff_is_recognised_and_a_list_is_not() {
        assert!(looks_like_diff("--- a/x\n+++ b/x\n@@ -1 +1 @@\n-one\n+two"));
        assert!(looks_like_diff("diff --git a/x b/x"));
        assert!(!looks_like_diff("- one\n- two\n+ three"));
        assert!(!looks_like_diff("nothing here"));
    }

    fn colour_of(l: &str) -> Option<Color> {
        diff_line(l).spans.first().and_then(|s| s.style.fg)
    }

    #[test]
    fn diff_lines_are_coloured_by_what_they_are() {
        assert_eq!(colour_of("+added"), Some(Color::Green));
        assert_eq!(colour_of("-removed"), Some(Color::Red));
        assert_eq!(colour_of("@@ -1 +1 @@"), Some(Color::Cyan));
        // The file headers are not additions and removals, whatever they start with.
        assert_eq!(colour_of("+++ b/x"), Some(Color::DarkGray));
        assert_eq!(colour_of("--- a/x"), Some(Color::DarkGray));
    }

    /// A tool result that happens to be a diff gets read as one.
    #[test]
    fn a_diff_shaped_tool_result_is_coloured() {
        let entry = Entry::ToolResult {
            name: "edit_file".into(),
            is_error: false,
            body: "@@ -1 +1 @@\n-old\n+new".into(),
        };
        let lines = entry.to_lines();
        assert!(lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.style.fg == Some(Color::Green))));
    }
}
