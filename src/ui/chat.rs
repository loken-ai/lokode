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
        }
    }

    pub fn to_lines(&self) -> Vec<Line<'static>> {
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
                result_lines(*is_error, body)
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
fn result_lines(is_error: bool, body: &str) -> Vec<Line<'static>> {
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
    let shown = if is_error {
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
