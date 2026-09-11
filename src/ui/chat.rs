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
    /// A change waiting to be agreed to, in the thread where the rest of the turn is.
    ///
    /// In the thread rather than over it: a modal hides the conversation the change has to
    /// be judged against, and the diff it covers is the one thing a reader wants beside
    /// it. It scrolls with everything else, and is replaced by what the tool did.
    Approval {
        tool: String,
        summary: String,
        diff: Option<String>,
        scope: String,
    },
    /// The model's scratchpad, accumulated while it streams.
    ///
    /// Shown dimmed and set apart, because it is the one thing in the pane the user must
    /// not mistake for the answer.
    Reasoning(String),
}

impl Entry {
    /// Whether there is more to this entry than what is shown.
    ///
    /// Only the entries that hold evidence: a result, a command's output, a scratchpad. A
    /// question and an answer are shown whole because they are what the pane is for.
    pub fn can_unfold(&self) -> bool {
        match self {
            Entry::ToolResult { body, .. } | Entry::ToolOutput { body, .. } => {
                body.lines().count() > BODY_LINES + 1
            }
            Entry::Reasoning(s) => s.lines().count() > BODY_LINES,
            _ => false,
        }
    }

    /// The entry as plain text, for taking the conversation out of the pane.
    ///
    /// The same content the screen carries, without the colour and the box drawing: what a
    /// person pastes into an issue or a message.
    pub fn to_text(&self) -> String {
        match self {
            Entry::User(s) => format!("> {s}"),
            Entry::Assistant(s) | Entry::AssistantStreaming(s) => s.clone(),
            Entry::ToolCall { name, args } => format!("{name}{}", call_subject(name, args)),
            Entry::ToolResult { is_error, body, .. } => match is_error {
                true => format!("error: {body}"),
                false => body.clone(),
            },
            Entry::ToolOutput { name, body } => format!("{name} …\n{body}"),
            Entry::Reasoning(s) => s.clone(),
            Entry::Notice(s) => format!("· {s}"),
            Entry::Approval { summary, diff, .. } => match diff {
                Some(d) => format!("? {summary}\n{d}"),
                None => format!("? {summary}"),
            },
        }
    }

    pub fn to_lines(&self) -> Vec<Line<'static>> {
        self.render(false, false)
    }

    /// The entry's lines, with what is shown decided by the caller.
    ///
    /// `open` shows the evidence whole rather than counted; `here` marks the entry the
    /// keys are pointing at, so moving between them is visible.
    pub fn render(&self, open: bool, here: bool) -> Vec<Line<'static>> {
        let mut lines = self.lines_inner(open);
        if here {
            // The mark goes in the left margin every line already leaves empty, so
            // pointing at an entry does not move its text.
            for line in lines.iter_mut() {
                let mark = Span::styled("▌", Style::default().fg(Color::Cyan));
                match line.spans.first_mut() {
                    Some(first) if first.content.starts_with(' ') => {
                        let rest: String = first.content.chars().skip(1).collect();
                        first.content = rest.into();
                        line.spans.insert(0, mark);
                    }
                    _ => line.spans.insert(0, mark),
                }
            }
        }
        lines
    }

    fn lines_inner(&self, open: bool) -> Vec<Line<'static>> {
        match self {
            // The person's own words, marked the way a shell marks a prompt rather than
            // with a header of their own: a line that says "you" over what you just typed
            // is a line spent saying nothing.
            Entry::User(s) => {
                let mut out: Vec<Line<'static>> = s
                    .lines()
                    .enumerate()
                    .map(|(i, l)| {
                        Line::from(vec![
                            Span::styled(
                                if i == 0 { "> " } else { "  " },
                                Style::default()
                                    .fg(Color::Cyan)
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(l.to_string(), Style::default().fg(Color::Cyan)),
                        ])
                    })
                    .collect();
                out.push(Line::from(""));
                out
            }
            Entry::Assistant(s) | Entry::AssistantStreaming(s) => {
                let mut out = prose(s);
                out.push(Line::from(""));
                out
            }
            // One line, the way a person would say it: the verb and the thing it acts
            // on. The arguments in full are what turned this pane into a wall of JSON,
            // and the one that matters is always the same one for a given tool.
            Entry::ToolCall { name, args } => vec![Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    name.clone(),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(call_subject(name, args), Style::default().fg(Color::Yellow)),
            ])],
            Entry::ToolResult {
                name,
                is_error,
                body,
            } => {
                let _ = name;
                result_lines(*is_error, body, open)
            }
            // `/diff` answers through a notice, and a wall of uncoloured diff is the thing
            // it is least useful as.
            Entry::Reasoning(s) => s
                .lines()
                .map(|l| {
                    Line::from(Span::styled(
                        format!("  {l}"),
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                    ))
                })
                .collect(),
            // Dimmed and set apart: it is what is happening, not what was said, and it
            // is replaced by the tool's own result the moment there is one.
            Entry::ToolOutput { name, body } => {
                let mut out = vec![Line::from(Span::styled(
                    format!("  {name} …"),
                    Style::default().fg(Color::DarkGray),
                ))];
                out.extend(body.lines().map(|l| {
                    Line::from(Span::styled(
                        format!("    {l}"),
                        Style::default().fg(Color::DarkGray),
                    ))
                }));
                out
            }
            // The one thing on screen that is waiting for the reader, so it is the one
            // thing that is neither dim nor the colour of an answer.
            Entry::Approval {
                summary,
                diff,
                scope,
                ..
            } => {
                let ask = Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD);
                let mut out = vec![Line::from(vec![
                    Span::styled("? ", ask),
                    Span::styled(summary.clone(), ask),
                ])];
                if let Some(d) = diff {
                    out.extend(
                        d.lines()
                            .take(match open {
                                true => usize::MAX,
                                false => DIFF_LINES,
                            })
                            .map(|l| indent(diff_line(l))),
                    );
                    let more = d.lines().count().saturating_sub(DIFF_LINES);
                    if more > 0 && !open {
                        out.push(Line::from(Span::styled(
                            format!("    … {more} more line(s), space to see them"),
                            Style::default().fg(Color::DarkGray),
                        )));
                    }
                }
                // The line a shell command is given is the one worth correcting, so the
                // key that does it is offered only where there is a command to correct.
                let fix = match summary.starts_with("$ ") {
                    true => "   x no, let me fix it",
                    false => "",
                };
                out.push(Line::from(Span::styled(
                    format!("  y once   a always ({scope})   n or esc no   e no, and say why{fix}"),
                    Style::default().fg(Color::Yellow),
                )));
                out.push(Line::from(""));
                out
            }
            // What the program says about itself, not what the model said. Two weights:
            // an ordinary remark recedes, and something that went wrong does not - an
            // error in the same grey as a footnote is an error nobody reads.
            Entry::Notice(s) => {
                if looks_like_diff(s) {
                    return s.lines().map(diff_line).collect();
                }
                let bad = says_something_went_wrong(s);
                let style = match bad {
                    true => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    false => Style::default().fg(Color::DarkGray),
                };
                let mark = if bad { "! " } else { "· " };
                s.lines()
                    .enumerate()
                    .map(|(i, l)| {
                        Line::from(Span::styled(
                            format!("{}{l}", if i == 0 { mark } else { "  " }),
                            style,
                        ))
                    })
                    .collect()
            }
        }
    }
}

/// Break rendered lines so that one line is one row on screen.
///
/// The pane used to hand its lines to a paragraph that wrapped them itself. That left the
/// program counting logical lines while the terminal showed rows, so scrolling was off by
/// however much had wrapped, and nothing could be said about what sits at a given row -
/// which is what selecting with the mouse needs to know.
///
/// Broken at a space where there is one, mid-word where there is not, and never mid-span:
/// a span carries its colour, and half a span carries half of it.
pub fn wrap(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return lines;
    }
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        if line_width(&line) <= width {
            out.push(line);
            continue;
        }
        let mut row: Vec<Span<'static>> = Vec::new();
        let mut used = 0usize;
        for span in line.spans {
            let style = span.style;
            let mut text = span.content.into_owned();
            while !text.is_empty() {
                let room = width - used;
                if text.chars().count() <= room {
                    used += text.chars().count();
                    row.push(Span::styled(text, style));
                    break;
                }
                let at = break_at(&text, room);
                let (head, tail) = text.split_at(at);
                if !head.is_empty() {
                    row.push(Span::styled(head.to_string(), style));
                }
                out.push(Line::from(std::mem::take(&mut row)));
                used = 0;
                text = tail.trim_start_matches(' ').to_string();
            }
        }
        out.push(Line::from(row));
    }
    out
}

/// Where to break `text` so that at most `room` columns are used.
///
/// At the last space that fits, so a word is not cut when it does not have to be; at
/// `room` when there is no space, because a line has to end somewhere.
fn break_at(text: &str, room: usize) -> usize {
    let hard = text
        .char_indices()
        .nth(room)
        .map(|(i, _)| i)
        .unwrap_or(text.len());
    // A space exactly where the room runs out means the whole of what fits is a word
    // boundary already.
    if text[hard..].starts_with(' ') {
        return hard;
    }
    match text[..hard].rfind(' ') {
        Some(at) if at > 0 => at,
        _ => hard,
    }
}

/// The columns a line takes.
pub fn line_width(line: &Line<'static>) -> usize {
    line.spans.iter().map(|s| s.content.chars().count()).sum()
}

/// A line as the plain text it shows, for taking a selection out of the pane.
pub fn line_text(line: &Line<'static>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Whether a remark is about something that went wrong.
///
/// Read off the wording rather than carried alongside it, because every one of these
/// reaches the pane as the same kind of message. The words are the ones the loop itself
/// uses when it reports a failure.
fn says_something_went_wrong(s: &str) -> bool {
    const BAD: [&str; 6] = [
        "backend error",
        "stopped:",
        "gave up",
        "failed",
        "could not",
        "refused",
    ];
    let head = s.lines().next().unwrap_or("").to_lowercase();
    BAD.iter().any(|b| head.contains(b))
}

/// What a call is about, in the words a person would use.
///
/// One argument carries the meaning of a call and the rest is machinery: which file, which
/// pattern, which command. Printing the arguments in full turned every step into a wall of
/// JSON, and the one that matters is the same one every time for a given tool.
fn call_subject(name: &str, args: &str) -> String {
    let parsed: serde_json::Value = serde_json::from_str(args).unwrap_or(serde_json::Value::Null);
    let field = |k: &str| parsed.get(k).and_then(|v| v.as_str()).map(str::to_string);
    let subject = match name {
        "grep" => field("pattern"),
        "bash" => field("cmd"),
        "glob" => field("pattern"),
        "web_fetch" => field("url"),
        "delegate" => field("task"),
        "run_tests" => field("target"),
        _ => field("path").or_else(|| field("file")),
    };
    // A tool with nothing to name still says how much it is doing.
    let subject = subject.or_else(|| {
        parsed
            .get("tasks")
            .and_then(|t| t.as_array())
            .map(|t| format!("{} task(s)", t.len()))
    });
    let subject = subject.or_else(|| {
        parsed
            .get("edits")
            .and_then(|t| t.as_array())
            .map(|t| format!("{} edit(s)", t.len()))
    });
    match subject {
        None => String::new(),
        Some(s) => {
            let one_line = s.replace('\n', " ");
            format!("({})", truncate(one_line.trim(), SUBJECT_CHARS))
        }
    }
}

/// A result in the shape of an answer rather than of a transcript.
///
/// The first line of a tool's own result is its answer; what follows is evidence. The
/// answer is always shown, the evidence up to a few lines, and the rest counted. An error
/// is shown whole because an error nobody can read is an error nobody can act on.
fn result_lines(is_error: bool, body: &str, open: bool) -> Vec<Line<'static>> {
    let colour = if is_error {
        Color::Red
    } else {
        Color::DarkGray
    };
    let lines: Vec<&str> = body.lines().collect();
    // Kept as it was written, leading spaces and all: a tool that numbers its lines lines
    // them up, and trimming the first one alone breaks the column.
    let head = lines.first().copied().unwrap_or("");
    let head = if head.trim().is_empty() && lines.len() > 1 {
        format!("{} line(s)", lines.len())
    } else if head.trim().is_empty() {
        "nothing".to_string()
    } else {
        truncate(head, SUBJECT_CHARS)
    };
    let mut out = vec![Line::from(vec![
        Span::styled("  ⤷ ", Style::default().fg(colour)),
        Span::styled(head, Style::default().fg(colour)),
    ])];
    let rest = &lines[lines.len().min(1)..];
    let shown = if is_error || open {
        rest.len()
    } else {
        rest.len().min(BODY_LINES)
    };
    let diff = looks_like_diff(body);
    for l in &rest[..shown] {
        out.push(match diff {
            true => indent(diff_line(l)),
            false => Line::from(Span::styled(
                format!("    {l}"),
                Style::default().fg(Color::DarkGray),
            )),
        });
    }
    if rest.len() > shown {
        out.push(Line::from(Span::styled(
            format!("    … {} more line(s)", rest.len() - shown),
            Style::default().fg(Color::DarkGray),
        )));
    }
    // One blank line after the answer: what follows is the model talking again, and the
    // two run together without it.
    out.push(Line::from(""));
    out
}

/// A rendered line, moved in under the result it belongs to.
fn indent(line: Line<'static>) -> Line<'static> {
    let mut spans = vec![Span::raw("    ")];
    spans.extend(line.spans);
    Line::from(spans)
}

/// How much of a subject or an answer fits on one line before it stops being one.
const SUBJECT_CHARS: usize = 96;

/// How much evidence is shown under an answer before it is counted instead.
const BODY_LINES: usize = 8;

/// How much of a diff is shown beside the question about it.
///
/// More than a result, because this is the one a person has to judge rather than read, and
/// less than the pane, because the conversation it is judged against has to stay visible
/// behind it.
const DIFF_LINES: usize = 16;

/// Whether a body is a unified diff.
///
/// Judged on the header a diff has and prose does not, rather than on leading `+` and `-`,
/// which a bulleted list also has.
fn looks_like_diff(s: &str) -> bool {
    s.lines()
        .any(|l| l.starts_with("@@ ") || l.starts_with("--- ") || l.starts_with("diff --git "))
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
    /// What is taken is what the pane shows between the two ends.
    ///
    /// Both orders, because dragging upward is as ordinary as dragging down, and the
    /// partial first and last rows are where an off-by-one would go unnoticed: the text
    /// still looks like the right text, with a character missing at each end.
    #[test]
    fn a_selection_is_the_text_between_its_ends() {
        let lines = vec![
            Line::from("first line"),
            Line::from("second line"),
            Line::from("third line"),
        ];
        let a = Spot { line: 0, column: 6 };
        let b = Spot { line: 2, column: 5 };
        assert_eq!(selected_text(&lines, a, b), "line\nsecond line\nthird");
        assert_eq!(selected_text(&lines, b, a), "line\nsecond line\nthird");
        // Within one row.
        let one = selected_text(
            &lines,
            Spot { line: 1, column: 0 },
            Spot { line: 1, column: 6 },
        );
        assert_eq!(one, "second");
    }

    /// Marking a selection must not lose or reorder what the line says.
    ///
    /// The spans are split at the edges of the selection so the colour already on the text
    /// survives under it; a split that dropped a character would show as text that reads
    /// correctly on screen and comes out of the clipboard wrong.
    #[test]
    fn marking_a_selection_keeps_every_character() {
        let line = Line::from(vec![
            Span::styled("abc", Style::default().fg(Color::Red)),
            Span::raw("def"),
            Span::styled("ghi", Style::default().fg(Color::Green)),
        ]);
        for (from, to) in [(0, 9), (2, 5), (4, 4), (0, 1), (8, 9), (3, 6)] {
            let marked = mark_selected(&line, from, to);
            assert_eq!(line_text(&marked), "abcdefghi", "from {from} to {to}");
        }
        // The chosen columns, and only those, carry the selection colour.
        let marked = mark_selected(&line, 2, 5);
        let chosen: String = marked
            .spans
            .iter()
            .filter(|s| s.style.bg == Some(Color::Blue))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(chosen, "cde");
    }

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

    /// One line in, one row per screen width out, and the text unchanged apart from the
    /// spaces a break falls on.
    #[test]
    fn wrapping_gives_one_row_per_line() {
        let line = Line::from(Span::raw("aaa bbb ccc ddd"));
        let rows = wrap(vec![line], 7);
        let text: Vec<String> = rows.iter().map(line_text).collect();
        assert_eq!(text, vec!["aaa bbb", "ccc ddd"]);
        assert!(rows.iter().all(|r| line_width(r) <= 7));
    }

    /// A word longer than the pane has to be cut: a line has to end somewhere.
    #[test]
    fn a_word_wider_than_the_pane_is_cut() {
        let rows = wrap(vec![Line::from(Span::raw("abcdefghij"))], 4);
        let text: Vec<String> = rows.iter().map(line_text).collect();
        assert_eq!(text, vec!["abcd", "efgh", "ij"]);
    }

    /// A break never falls inside a span, because a span carries its colour and half a
    /// span carries half of it.
    #[test]
    fn colour_survives_a_break() {
        let line = Line::from(vec![
            Span::styled("red red red", Style::default().fg(Color::Red)),
            Span::styled(" green", Style::default().fg(Color::Green)),
        ]);
        let rows = wrap(vec![line], 6);
        for row in &rows {
            for span in &row.spans {
                assert!(span.style.fg.is_some(), "a span lost its colour: {row:?}");
            }
        }
        // Nothing is lost but the spaces a break falls on.
        let squashed: String = rows
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("")
            .split_whitespace()
            .collect();
        assert_eq!(squashed, "redredredgreen");
        assert!(rows.iter().all(|r| line_width(r) <= 6));
    }

    #[test]
    fn a_line_that_fits_is_left_alone() {
        let rows = wrap(vec![Line::from(Span::raw("short"))], 40);
        assert_eq!(rows.len(), 1);
        assert_eq!(line_text(&rows[0]), "short");
    }

    /// An error in the same grey as a footnote is an error nobody reads.
    #[test]
    fn a_remark_about_a_failure_is_not_a_footnote() {
        assert!(says_something_went_wrong(
            "backend error: connection refused"
        ));
        assert!(says_something_went_wrong(
            "stopped: 3 steps in a row that got nowhere"
        ));
        assert!(!says_something_went_wrong(
            "12s · 2 step(s) · nothing changed"
        ));
        assert!(!says_something_went_wrong("backend http://127.0.0.1:11435"));
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

/// One end of a selection: which line, and how many columns into it.
///
/// Counted in lines of the wrapped pane rather than in entries, because that is what the
/// mouse points at: a row on screen is a line here, which is the whole reason the wrapping
/// is done in this file rather than left to the widget.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct Spot {
    pub line: usize,
    pub column: usize,
}

/// The text between two spots, as the pane shows it.
///
/// Ends given in either order: what the mouse reports is where it was pressed and where it
/// is now, and dragging upward is as ordinary as dragging down.
pub fn selected_text(lines: &[Line<'static>], a: Spot, b: Spot) -> String {
    let (from, to) = if a <= b { (a, b) } else { (b, a) };
    let mut out = Vec::new();
    for row in from.line..=to.line.min(lines.len().saturating_sub(1)) {
        let Some(line) = lines.get(row) else { break };
        let text: Vec<char> = line_text(line).chars().collect();
        let start = if row == from.line { from.column } else { 0 };
        let end = if row == to.line {
            to.column.min(text.len())
        } else {
            text.len()
        };
        if start < end {
            out.push(text[start..end].iter().collect::<String>());
        } else {
            out.push(String::new());
        }
    }
    // A trailing run of blank rows is the empty part of the pane under the conversation,
    // not something the person meant to take.
    while out.last().is_some_and(|l| l.trim().is_empty()) && out.len() > 1 {
        out.pop();
    }
    out.join("\n")
}

/// `line` with the columns between `from` and `to` shown as chosen.
///
/// Spans are split where the selection starts and ends: a span carries its colour, and the
/// selection has to sit on top of whatever colour is already there rather than replace it.
pub fn mark_selected(line: &Line<'static>, from: usize, to: usize) -> Line<'static> {
    if from >= to {
        return line.clone();
    }
    let chosen = Style::default()
        .bg(Color::Blue)
        .add_modifier(Modifier::BOLD);
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut at = 0usize;
    for span in &line.spans {
        let chars: Vec<char> = span.content.chars().collect();
        let mut cut = 0usize;
        while cut < chars.len() {
            let here = at + cut;
            // How far this run goes before it crosses into or out of the selection.
            let edge = if here < from {
                from - here
            } else if here < to {
                to - here
            } else {
                chars.len() - cut
            };
            let take = edge.min(chars.len() - cut);
            let text: String = chars[cut..cut + take].iter().collect();
            let style = if here >= from && here < to {
                span.style.patch(chosen)
            } else {
                span.style
            };
            out.push(Span::styled(text, style));
            cut += take;
        }
        at += chars.len();
    }
    Line::from(out)
}
