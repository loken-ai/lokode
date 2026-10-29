use std::time::Duration;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::agent::agent_loop::{Origin, Receipt};
use crate::ui::theme::theme;

/// One call of a tool: what it acts on, what it is printing, and what it came to.
#[derive(Debug, Clone, Default)]
pub struct Step {
    /// The id the call and its result share. Empty for a command the person ran.
    pub id: String,
    pub name: String,
    /// What the call acts on, in a few words.
    pub subject: String,
    /// What it has printed so far, while it runs.
    pub live: String,
    /// What it came to, once it has.
    pub outcome: Option<Outcome>,
    /// The steps of a session this call ran, as it ran them.
    pub children: Vec<Step>,
    /// What that session answered.
    pub said: String,
    /// When the call was asked for.
    pub started: Option<std::time::Instant>,
}

/// What a step came to.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub is_error: bool,
    pub body: String,
    pub took: Duration,
    pub origin: Origin,
}

impl Step {
    /// A call just asked for, with the arguments the model gave it.
    pub fn called(id: String, name: String, args: &str) -> Self {
        let subject = call_subject(&name, args);
        Self {
            id,
            name,
            subject,
            started: Some(std::time::Instant::now()),
            ..Default::default()
        }
    }

    /// A command the person ran themselves, already over.
    pub fn ran_by_hand(command: String, is_error: bool, body: String, took: Duration) -> Self {
        Self {
            id: String::new(),
            name: "$".into(),
            subject: command,
            live: String::new(),
            outcome: Some(Outcome {
                is_error,
                body,
                took,
                origin: Origin::Ran,
            }),
            ..Default::default()
        }
    }

    /// The page a step opens on: its call, then for a session it ran each of that
    /// session's steps with what it came to, then what it answered or printed, whole.
    pub fn page(&self) -> String {
        // The page's band names the call, so the page starts with what it did.
        let mut out = String::new();
        if !self.children.is_empty() {
            out.push_str("### Steps\n\n");
            for child in &self.children {
                let (mark, said) = match &child.outcome {
                    None => ("◌", "running".to_string()),
                    Some(o) if o.is_error => ("✕", "failed".to_string()),
                    Some(o) => ("●", o.body.lines().next().unwrap_or("").trim().to_string()),
                };
                let took = child
                    .outcome
                    .as_ref()
                    .filter(|o| o.took >= SHOWN_FROM)
                    .map(|o| format!(" · {}", how_long(o.took)))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "{mark} **{}** `{}` · {said}{took}\n\n",
                    child.name,
                    child.subject.replace('`', "'")
                ));
                if let Some(o) = &child.outcome {
                    let body: Vec<&str> = o.body.lines().skip(1).take(PAGE_EVIDENCE).collect();
                    if !body.is_empty() {
                        out.push_str(&format!("```\n{}\n```\n\n", body.join("\n")));
                    }
                }
            }
        }
        let answer = match (&self.outcome, self.said.trim().is_empty()) {
            (_, false) => self.said.clone(),
            (Some(o), true) => o.body.clone(),
            (None, true) => self.live.clone(),
        };
        if !answer.trim().is_empty() {
            match self.children.is_empty() {
                true => out.push_str(&format!("```\n{answer}\n```\n")),
                false => out.push_str(&format!("### Answer\n\n{answer}\n")),
            }
        }
        out
    }
}

/// A single rendered entry in the chat scrollback.
#[derive(Debug, Clone)]
pub enum Entry {
    User(String),
    Assistant(String),
    AssistantStreaming(String),
    /// One call of a tool, from the moment it is asked for to what it came to.
    Step(Step),
    Notice(String),
    /// What a turn came to, closing it.
    Receipt(Receipt),
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
            Entry::Step(step) => {
                !step.children.is_empty()
                    || match &step.outcome {
                        Some(o) => o.body.lines().count() > BODY_LINES + 1,
                        None => step.live.lines().count() > RUNNING_LINES,
                    }
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
            Entry::Step(step) => {
                let said = match &step.outcome {
                    Some(o) if o.is_error => format!("error: {}", o.body),
                    Some(o) => o.body.clone(),
                    None => step.live.clone(),
                };
                format!("{} {}\n{said}", step.name, step.subject)
            }
            Entry::Reasoning(s) => s.clone(),
            Entry::Notice(s) => format!("· {s}"),
            Entry::Receipt(r) => format!("✓ {}", r.describe()),
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
        self.render_within(open, here, 0)
    }

    /// The entry's lines for a pane `width` columns wide, which a step uses to put its
    /// outcome at the right edge. Zero leaves the outcome beside the call.
    pub fn render_within(&self, open: bool, here: bool, width: usize) -> Vec<Line<'static>> {
        self.render_joined(open, here, width, false)
    }

    /// As [`Entry::render_within`], with `joined` saying a step follows this one in the
    /// same turn, so the rail carries on through the row between them.
    pub fn render_joined(
        &self,
        open: bool,
        here: bool,
        width: usize,
        joined: bool,
    ) -> Vec<Line<'static>> {
        let mut lines = match self {
            Entry::Step(step) => step_lines(step, open, width, joined),
            Entry::Receipt(r) => receipt_lines(r, width, here),
            Entry::User(s) => user_lines(s, width),
            Entry::Assistant(s) | Entry::AssistantStreaming(s) => {
                let mut out = crate::ui::markdown::render(s, width);
                out.push(Line::from(""));
                out
            }
            _ => self.lines_inner(open, width),
        };
        if here {
            // The mark goes in the left margin every line already leaves empty, so
            // pointing at an entry does not move its text.
            for line in lines.iter_mut() {
                let mark = Span::styled("▌", Style::default().fg(theme().accent));
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

    fn lines_inner(&self, open: bool, width: usize) -> Vec<Line<'static>> {
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
                                    .fg(theme().accent)
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(l.to_string(), Style::default().fg(theme().accent)),
                        ])
                    })
                    .collect();
                out.push(Line::from(""));
                out
            }
            Entry::Assistant(s) | Entry::AssistantStreaming(s) => {
                let mut out = crate::ui::markdown::render(s, 0);
                out.push(Line::from(""));
                out
            }
            Entry::Step(step) => step_lines(step, open, 0, false),
            Entry::Receipt(r) => receipt_lines(r, 0, false),
            // `/diff` answers through a notice, and a wall of uncoloured diff is the thing
            // it is least useful as.
            Entry::Reasoning(s) => s
                .lines()
                .map(|l| {
                    Line::from(Span::styled(
                        format!("  {l}"),
                        theme().dim.add_modifier(Modifier::ITALIC),
                    ))
                })
                .collect(),
            // The one thing on screen that is waiting for the reader, so it is the one
            // thing that is neither dim nor the colour of an answer.
            Entry::Approval {
                summary,
                diff,
                scope,
                ..
            } => {
                let t = theme();
                let ask = Style::default().fg(t.warn).add_modifier(Modifier::BOLD);
                let mut out = vec![Line::from(vec![
                    Span::raw(" "),
                    Span::styled("◉", Style::default().fg(t.warn)),
                    Span::raw(" "),
                    Span::styled(summary.clone(), ask),
                    Span::styled("  waiting for you", t.dim),
                ])];
                if let Some(d) = diff {
                    let rows = crate::ui::diff::rows(d, width.saturating_sub(4));
                    let more = rows.len().saturating_sub(DIFF_LINES);
                    out.extend(
                        rows.into_iter()
                            .take(match open {
                                true => usize::MAX,
                                false => DIFF_LINES,
                            })
                            .map(indent),
                    );
                    if more > 0 && !open {
                        out.push(Line::from(Span::styled(
                            format!("    … {more} more line(s), space to see them"),
                            theme().dim,
                        )));
                    }
                }
                // The keys as badges, the letter on the colour of the question. The line a
                // shell command is given is the one worth correcting, so the key that does it
                // is offered only where there is a command to correct.
                let badge = |k: &str| {
                    Span::styled(
                        format!(" {k} "),
                        Style::default()
                            .fg(t.ink)
                            .bg(t.warn)
                            .add_modifier(Modifier::BOLD),
                    )
                };
                let mut keys = vec![
                    Span::raw("   "),
                    badge("y"),
                    Span::styled(" once  ", t.dim),
                    badge("a"),
                    Span::styled(format!(" always in {scope}  "), t.dim),
                    badge("n"),
                    Span::styled(" no  ", t.dim),
                    badge("e"),
                    Span::styled(" no, and say why", t.dim),
                ];
                if summary.starts_with("$ ") {
                    keys.push(Span::raw("  "));
                    keys.push(badge("x"));
                    keys.push(Span::styled(" no, let me fix it", t.dim));
                }
                out.push(Line::from(""));
                out.push(Line::from(keys));
                // On a raised surface of its own, where the terminal can show one: the one
                // thing on screen that waits for the reader.
                if let (Some(tint), true) = (t.tint, width > 0) {
                    for line in out.iter_mut() {
                        for span in line.spans.iter_mut() {
                            if span.style.bg.is_none() {
                                span.style = span.style.bg(tint);
                            }
                        }
                        let used = line_width(line);
                        if width > used {
                            line.spans.push(Span::styled(
                                " ".repeat(width - used),
                                Style::default().bg(tint),
                            ));
                        }
                    }
                }
                out.push(Line::from(""));
                out
            }
            // What the program says about itself, not what the model said. Two weights:
            // an ordinary remark recedes, and something that went wrong does not - an
            // error in the same grey as a footnote is an error nobody reads.
            Entry::Notice(s) => {
                if looks_like_diff(s) {
                    return crate::ui::diff::rows(s, width);
                }
                let bad = says_something_went_wrong(s);
                let style = match bad {
                    true => Style::default()
                        .fg(theme().bad)
                        .add_modifier(Modifier::BOLD),
                    false => theme().dim,
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
        Some(s) => clip(s.replace('\n', " ").trim(), SUBJECT_CHARS),
    }
}

/// A step as rows: the call on one line with what it came to at the right edge, and the
/// evidence under it on the rail.
///
/// The first line of a tool's own result is its answer and goes on the call's line. What
/// follows is evidence: up to a few lines, the rest counted. An error is shown whole. While
/// the call runs, the end of what it is printing stands in for the evidence.
/// A step's row: its mark, its call and what it came to, against the edge of `width`.
fn step_header(step: &Step, width: usize) -> Line<'static> {
    let t = theme();
    let (glyph, glyph_style) = match &step.outcome {
        None => ("◌", Style::default().fg(t.accent)),
        Some(o) if o.is_error && o.body.starts_with("user denied") => {
            ("✕", Style::default().fg(t.warn))
        }
        Some(o) if o.is_error => ("✕", Style::default().fg(t.bad)),
        Some(_) => ("●", Style::default().fg(t.good)),
    };
    let mut left = vec![
        Span::raw(" "),
        Span::styled(glyph, glyph_style),
        Span::raw(" "),
        Span::styled(
            step.name.clone(),
            Style::default().fg(t.tool).add_modifier(Modifier::BOLD),
        ),
    ];
    if !step.subject.is_empty() {
        left.push(Span::styled(
            format!("  {}", step.subject),
            Style::default().fg(t.tool),
        ));
    }
    let mut right: Vec<Span<'static>> = Vec::new();
    if let Some(o) = &step.outcome {
        let head = o.body.lines().next().unwrap_or("");
        // What is left of the row once the call and the duration are on it, so the row
        // stays one line.
        let used: usize = left
            .iter()
            .map(|s| s.content.chars().count())
            .sum::<usize>()
            + 2
            + "guessed  ".len()
            + match o.took >= SHOWN_FROM {
                true => 2 + how_long(o.took).chars().count(),
                false => 0,
            };
        let room = width
            .saturating_sub(used)
            .clamp(SAID_AT_LEAST, SUBJECT_CHARS);
        let said = match (o.is_error, head.trim().is_empty(), o.body.lines().count()) {
            (true, _, _) => "failed".to_string(),
            (false, true, n) if n > 1 => format!("{n} line(s)"),
            (false, true, _) => "nothing".to_string(),
            (false, false, _) => clip(head.trim(), room),
        };
        match o.origin {
            Origin::Guessed => right.push(Span::styled("guessed  ", Style::default().fg(t.accent))),
            Origin::Repeated => right.push(Span::styled("repeat  ", t.dim)),
            Origin::Ran | Origin::Withheld => {}
        }
        right.push(Span::styled(
            said,
            match o.is_error {
                true => Style::default().fg(t.bad),
                false => t.dim,
            },
        ));
        if o.took >= SHOWN_FROM {
            right.push(Span::styled(format!("  {}", how_long(o.took)), t.dim));
        }
    }
    beside(left, right, width)
}

fn step_lines(step: &Step, open: bool, width: usize, joined: bool) -> Vec<Line<'static>> {
    let t = theme();
    let width = width.min(STEP_AT_MOST);
    let mut out = vec![step_header(step, width)];

    let rail = |text: String, style: Style| {
        Line::from(vec![
            Span::raw(" "),
            Span::styled("▏", t.dim),
            Span::styled(format!("   {text}"), style),
        ])
    };
    let rail_line = |line: Line<'static>| {
        let mut spans = vec![Span::raw(" "), Span::styled("▏", t.dim), Span::raw("   ")];
        spans.extend(line.spans);
        Line::from(spans)
    };
    // The steps of a session the call ran: while it runs, the latest of them, and once it is
    // over, a count that unfolds into all of them.
    if !step.children.is_empty() {
        let n = step.children.len();
        let running = step.outcome.is_none();
        let first = match (open, running) {
            (true, _) => 0,
            (false, true) => n.saturating_sub(RUNNING_LINES),
            (false, false) => n,
        };
        if first > 0 && running {
            out.push(rail(format!("… {first} earlier step(s)"), t.dim));
        }
        for child in &step.children[first..] {
            out.push(rail_line(step_header(child, width.saturating_sub(5))));
        }
        if !open && !running {
            out.push(rail(format!("{n} step(s)"), t.dim));
        }
    }
    match &step.outcome {
        None => {
            let held = step.live.lines().count();
            let shown = match open {
                true => held,
                false => held.min(RUNNING_LINES),
            };
            if shown < held {
                out.push(rail(
                    format!("… {} earlier line(s), space to see them", held - shown),
                    t.dim,
                ));
            }
            out.extend(
                step.live
                    .lines()
                    .skip(held - shown)
                    .map(|l| rail(l.to_string(), t.dim)),
            );
        }
        Some(o) if o.is_error => {
            out.extend(
                o.body
                    .lines()
                    .map(|l| rail(l.to_string(), Style::default().fg(t.bad))),
            );
        }
        Some(o) => {
            let rest: Vec<&str> = o.body.lines().skip(1).collect();
            let evidence: Vec<Line<'static>> = match looks_like_diff(&o.body) {
                true => crate::ui::diff::rows(&rest.join("\n"), width.saturating_sub(5))
                    .into_iter()
                    .map(rail_line)
                    .collect(),
                false => rest.iter().map(|l| rail(l.to_string(), t.dim)).collect(),
            };
            let shown = match open {
                true => evidence.len(),
                false => evidence.len().min(BODY_LINES),
            };
            let more = evidence.len() - shown;
            out.extend(evidence.into_iter().take(shown));
            if more > 0 {
                out.push(rail(
                    format!("… {more} more line(s), space to see them"),
                    t.dim,
                ));
            }
        }
    }
    // The row after a step carries the rail on when another step of the turn follows.
    out.push(match joined {
        true => Line::from(vec![Span::raw(" "), Span::styled("▏", t.dim)]),
        false => Line::from(""),
    });
    out
}

/// The person's own words, opening a turn: on a band of the accent where the terminal can
/// show one, so each turn starts where the eye can find it, and marked with the prompt's
/// sign otherwise.
fn user_lines(s: &str, width: usize) -> Vec<Line<'static>> {
    let t = theme();
    let mut out = Vec::new();
    match t.band {
        Some(band) => {
            let surface = Style::default().bg(band);
            let room = width.saturating_sub(3).max(1);
            for (i, l) in s.lines().enumerate() {
                for (k, row) in wrap(vec![Line::from(l.to_string())], room)
                    .into_iter()
                    .enumerate()
                {
                    let mark = match (i, k) {
                        (0, 0) => {
                            Span::styled(" › ", surface.fg(t.accent).add_modifier(Modifier::BOLD))
                        }
                        _ => Span::styled("   ", surface),
                    };
                    let mut spans = vec![mark];
                    spans.extend(
                        row.spans
                            .into_iter()
                            .map(|x| Span::styled(x.content, surface.add_modifier(Modifier::BOLD))),
                    );
                    out.push(pad_to(spans, width, surface));
                }
            }
        }
        None => {
            for (i, l) in s.lines().enumerate() {
                out.push(Line::from(vec![
                    Span::styled(
                        if i == 0 { "› " } else { "  " },
                        Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(l.to_string(), Style::default().fg(t.accent)),
                ]));
            }
        }
    }
    out.push(Line::from(""));
    out
}

/// `spans` filled out to `width` in `style`, so a band's colour runs across the pane.
fn pad_to(mut spans: Vec<Span<'static>>, width: usize, style: Style) -> Line<'static> {
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    if width > used {
        spans.push(Span::styled(" ".repeat(width - used), style));
    }
    Line::from(spans)
}

/// The line that closes a turn: how long, how many steps, what was read and written, and
/// what changed, with the commands that look at a change at the right edge when there is one.
fn receipt_lines(r: &Receipt, width: usize, here: bool) -> Vec<Line<'static>> {
    let t = theme();
    let mut said = vec![how_long(r.took)];
    if r.steps > 0 {
        said.push(format!("{} step(s)", r.steps));
    }
    if r.tokens_in + r.tokens_out > 0 {
        said.push(format!(
            "{} read, {} written",
            tokens(r.tokens_in),
            tokens(r.tokens_out)
        ));
    }
    said.push(match r.files.len() {
        0 => "nothing changed".to_string(),
        1 => format!("changed {}", r.files[0]),
        n => format!("changed {n} files"),
    });
    if let Some(e) = &r.energy {
        said.push(e.describe());
    }
    let left = vec![
        Span::raw(" "),
        Span::styled("✓", Style::default().fg(t.good)),
        Span::styled(format!(" {}", said.join(" · ")), t.dim),
    ];
    // Pointed at, the receipt is acted on by a key: put the turn back, or see what it
    // changed. Otherwise it names the commands that do the same.
    let right = match (r.files.is_empty(), here) {
        (true, _) => Vec::new(),
        (false, true) => vec![
            Span::styled("u", t.key),
            Span::styled(" put this turn back  ", t.dim),
            Span::styled("d", t.key),
            Span::styled(" see its diff", t.dim),
        ],
        (false, false) => vec![
            Span::styled("/undo", t.key),
            Span::styled(" put back  ", t.dim),
            Span::styled("/diff", t.key),
            Span::styled(" see it", t.dim),
        ],
    };
    // The commands are a convenience: they go when the row has no room for them.
    let wide = |spans: &[Span<'static>]| -> usize {
        spans.iter().map(|s| s.content.chars().count()).sum()
    };
    let right = match width > 0 && wide(&left) + 2 + wide(&right) > width {
        true => Vec::new(),
        false => right,
    };
    vec![beside(left, right, width), Line::from("")]
}

/// A count of tokens in a few characters: 940, 8.4k, 1.2M.
pub(crate) fn tokens(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

/// `left` and `right` on one row, `right` against the edge of `width` when both fit,
/// two spaces after `left` when they do not or when there is no width to align to.
fn beside(left: Vec<Span<'static>>, right: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let wide = |spans: &[Span<'static>]| -> usize {
        spans.iter().map(|s| s.content.chars().count()).sum()
    };
    let (l, r) = (wide(&left), wide(&right));
    let mut spans = left;
    if r == 0 {
        return Line::from(spans);
    }
    let gap = match width {
        0 => 2,
        w if l + 2 + r <= w => w - l - r,
        _ => 2,
    };
    spans.push(Span::raw(" ".repeat(gap)));
    spans.extend(right);
    Line::from(spans)
}

/// Seconds to a tenth up to a minute, then minutes and seconds.
pub fn how_long(d: Duration) -> String {
    let s = d.as_secs_f64();
    match s < 60.0 {
        true => format!("{s:.1}s"),
        false => format!("{}m{:02}s", (s / 60.0) as u64, (s % 60.0) as u64),
    }
}

/// `s` cut to `n` characters on one line, ending in an ellipsis when it was cut.
fn clip(s: &str, n: usize) -> String {
    match s.chars().count() > n {
        true => s.chars().take(n.saturating_sub(1)).chain(['…']).collect(),
        false => s.to_string(),
    }
}

/// A rendered line, moved in under the result it belongs to.
fn indent(line: Line<'static>) -> Line<'static> {
    let mut spans = vec![Span::raw("    ")];
    spans.extend(line.spans);
    Line::from(spans)
}

/// How much of a subject or an answer fits on one line before it stops being one.
const SUBJECT_CHARS: usize = 96;

/// The fewest characters of what a step came to that its row shows, however full it is.
const SAID_AT_LEAST: usize = 12;

/// How many lines of each step's evidence a step's page shows.
const PAGE_EVIDENCE: usize = 40;

/// How long a step has to take before its duration is worth showing.
const SHOWN_FROM: Duration = Duration::from_millis(100);

/// How much evidence is shown under an answer before it is counted instead.
const BODY_LINES: usize = 8;

/// How much of a diff is shown beside the question about it.
///
/// More than a result, because this is the one a person has to judge rather than read, and
/// less than the pane, because the conversation it is judged against has to stay visible
/// behind it.
/// How much of a tool's output is shown while it is still running.
const RUNNING_LINES: usize = 6;

/// The widest a step's row is laid out, so its outcome stays near the call on a wide pane.
const STEP_AT_MOST: usize = 100;

const DIFF_LINES: usize = 16;

/// Whether a body is a unified diff.
///
/// Judged on the header a diff has and prose does not, rather than on leading `+` and `-`,
/// which a bulleted list also has.
pub(crate) fn looks_like_diff(s: &str) -> bool {
    s.lines()
        .any(|l| l.starts_with("@@ ") || l.starts_with("--- ") || l.starts_with("diff --git "))
}

#[cfg(test)]
mod tests {
    use ratatui::style::Color;

    /// Pointing at the entry a word is in leaves the word itself to be found by eye,
    /// which on a screenful of output is the part that takes the time.
    #[test]
    fn what_was_searched_for_is_picked_out_where_it_is() {
        let line = Line::from("the session compacts the session");
        let marked = mark_found(&line, "session");
        // Same text, more spans: the two occurrences are cut out of the run around them.
        assert_eq!(line_text(&marked), line_text(&line));
        let lit: Vec<&str> = marked
            .spans
            .iter()
            .filter(|s| s.style.bg == Some(Color::Yellow))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(lit, vec!["session", "session"]);
        // Case is not what a person searching means.
        assert_eq!(
            mark_found(&Line::from("Session"), "session")
                .spans
                .iter()
                .filter(|s| s.style.bg == Some(Color::Yellow))
                .count(),
            1
        );
        // Nothing to look for leaves the line alone.
        assert_eq!(mark_found(&line, "").spans.len(), line.spans.len());
    }
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

    /// A tool result that happens to be a diff gets read as one.
    #[test]
    fn a_diff_shaped_tool_result_is_coloured() {
        let entry = Entry::Step(Step {
            id: "1".into(),
            name: "edit_file".into(),
            subject: "src/a.rs".into(),
            live: String::new(),
            outcome: Some(Outcome {
                is_error: false,
                body: "edited src/a.rs\n@@ -1 +1 @@\n-old\n+new".into(),
                took: Duration::ZERO,
                origin: Origin::Ran,
            }),
            ..Default::default()
        });
        let lines = entry.to_lines();
        assert!(lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.style.fg == Some(Color::Green))));
    }

    fn step(outcome: Option<Outcome>) -> Step {
        Step {
            id: "1".into(),
            name: "read_file".into(),
            subject: "src/mcp/client.rs".into(),
            live: String::new(),
            outcome,
            ..Default::default()
        }
    }

    fn done(body: &str, took: Duration, origin: Origin) -> Option<Outcome> {
        Some(Outcome {
            is_error: false,
            body: body.into(),
            took,
            origin,
        })
    }

    /// A step's call and what it came to share a row, the outcome against the right edge.
    #[test]
    fn a_step_is_one_row_with_its_outcome_at_the_edge() {
        let rows = step_lines(
            &step(done(
                "212 lines\n1 use std;",
                Duration::from_millis(1500),
                Origin::Ran,
            )),
            false,
            60,
            false,
        );
        let head = line_text(&rows[0]);
        assert!(
            head.starts_with(" ● read_file  src/mcp/client.rs"),
            "{head}"
        );
        assert!(head.ends_with("212 lines  1.5s"), "{head}");
        assert_eq!(head.chars().count(), 60);
        assert_eq!(line_text(&rows[1]), " ▏   1 use std;");
    }

    /// A quick step does not say how long it took, and a guessed one says it was guessed.
    #[test]
    fn a_guessed_step_says_so_and_a_quick_one_keeps_quiet() {
        let rows = step_lines(
            &step(done("12 lines", Duration::ZERO, Origin::Guessed)),
            false,
            0,
            false,
        );
        let head = line_text(&rows[0]);
        assert!(head.ends_with("guessed  12 lines"), "{head}");
    }

    /// A step still running shows the end of what it prints, and a failed one its error.
    #[test]
    fn a_running_step_shows_its_tail_and_a_failed_one_its_error() {
        let mut running = step(None);
        running.live = (0..30)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let rows = step_lines(&running, false, 0, false);
        assert!(line_text(&rows[0]).starts_with(" ◌ read_file"));
        assert!(line_text(&rows[1]).contains("earlier line(s)"));
        assert_eq!(line_text(&rows[rows.len() - 2]), " ▏   line 29");
        let failed = step(Some(Outcome {
            is_error: true,
            body: "no such file\ntried src/mcp".into(),
            took: Duration::ZERO,
            origin: Origin::Ran,
        }));
        let rows = step_lines(&failed, false, 0, false);
        assert!(line_text(&rows[0]).starts_with(" ✕ read_file"));
        assert!(line_text(&rows[0]).ends_with("failed"));
        assert_eq!(line_text(&rows[1]), " ▏   no such file");
        assert_eq!(line_text(&rows[2]), " ▏   tried src/mcp");
    }

    /// A receipt says how long, how much and what changed on one row, and offers the
    /// commands that look at a change only when there is one.
    #[test]
    fn a_receipt_closes_a_turn_on_one_row() {
        let mut r = Receipt {
            turn: 3,
            took: Duration::from_secs(38),
            steps: 7,
            tokens_in: 8400,
            tokens_out: 940,
            files: vec!["src/a.rs".into()],
            energy: None,
        };
        let head = line_text(&receipt_lines(&r, 110, false)[0]);
        assert!(
            head.starts_with(" ✓ 38.0s · 7 step(s) · 8.4k read, 940 written · changed src/a.rs"),
            "{head}"
        );
        assert!(head.ends_with("/diff see it"), "{head}");
        r.files.clear();
        let head = line_text(&receipt_lines(&r, 110, false)[0]);
        assert!(head.ends_with("nothing changed"), "{head}");
        assert_eq!(tokens(1_250_000), "1.2M");
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
pub fn mark_selected(line: &Line<'static>, from: usize, to: usize) -> Line<'static> {
    mark_range(line, from, to, theme().selected)
}

/// `line` with every occurrence of `needle` picked out.
///
/// Pointing at the entry a word is in leaves the word itself to be found by eye, which on
/// a screenful of output is the part that takes the time.
pub fn mark_found(line: &Line<'static>, needle: &str) -> Line<'static> {
    if needle.is_empty() {
        return line.clone();
    }
    let hay: Vec<char> = line_text(line).to_lowercase().chars().collect();
    let want: Vec<char> = needle.to_lowercase().chars().collect();
    if want.is_empty() || hay.len() < want.len() {
        return line.clone();
    }
    let style = theme().found;
    let mut out = line.clone();
    let mut at = 0usize;
    while at + want.len() <= hay.len() {
        if hay[at..at + want.len()] == want[..] {
            out = mark_range(&out, at, at + want.len(), style);
            at += want.len();
        } else {
            at += 1;
        }
    }
    out
}

/// `line` with the columns between `from` and `to` shown in `marked`.
///
/// Spans are split where the range starts and ends: a span carries its colour, and what is
/// put on top has to sit over whatever colour is already there rather than replace it.
pub(crate) fn mark_range(
    line: &Line<'static>,
    from: usize,
    to: usize,
    marked: Style,
) -> Line<'static> {
    if from >= to {
        return line.clone();
    }
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
                span.style.patch(marked)
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
