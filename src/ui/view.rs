//! One frame of the interface, drawn from the state.

use std::path::Path;

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use ratatui_textarea::TextArea;

use crate::ui::state::{UiState, SAID_FOR};
use crate::ui::status::Status;
use crate::ui::theme::theme;

/// Draw one frame: the conversation, the input, the status line and the keys.
pub(crate) fn draw(
    f: &mut Frame,
    state: &mut UiState,
    input: &TextArea,
    root: &Path,
    max_steps: usize,
) {
    // No borders anywhere. They cost four columns and two lines, they land in
    // every selection made with the mouse, and they separate nothing that a blank
    // line and a prompt mark do not separate better.
    //
    // The input grows with what is being typed, up to a few lines, so a long
    // instruction is visible while it is written instead of scrolling inside a box
    // of fixed height.
    let typed = input.lines().len().clamp(1, INPUT_MAX_LINES) as u16;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(typed),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(f.area());
    // The band at the top, then the same areas as before it.
    let header = chunks[0];
    let chunks = &chunks[1..];

    // Wrapped entry by entry, so where each one begins is known. Pointing at a
    // step said nothing about where it is: the mark went on an entry that could be
    // a screen and a half above the pane, and the only way to see it was to guess
    // how far to scroll.
    // The last column is kept for the scrollbar, so it never sits over text.
    // The rail takes a column of its own from a wide enough screen, and opens over the
    // conversation on a narrow one when asked for.
    let wide = f.area().width >= crate::ui::rail::WIDE_FROM;
    let rail_shown = state.rail.unwrap_or(wide);
    state.rail_now = rail_shown;
    let (pane_area, rail_area) = match (rail_shown, wide) {
        (true, true) => {
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Min(1),
                    Constraint::Length(crate::ui::rail::WIDTH),
                ])
                .split(chunks[0]);
            (columns[0], Some(columns[1]))
        }
        (true, false) => {
            let w = crate::ui::rail::WIDTH.min(chunks[0].width);
            let over = ratatui::layout::Rect {
                x: chunks[0].x + chunks[0].width - w,
                width: w,
                ..chunks[0]
            };
            (chunks[0], Some(over))
        }
        (false, _) => (chunks[0], None),
    };
    // At most a comfortable reading width: on a wide screen the conversation keeps its
    // measure, and its surfaces do not run the width of the screen.
    let width = (pane_area.width.saturating_sub(1) as usize).min(READING_AT_MOST);
    let mut pane = crate::ui::thread::Pane::default();
    for i in 0..state.entries.len() {
        // A step followed by another step of the turn keeps the rail between them.
        let joined = matches!(state.entries[i], crate::ui::chat::Entry::Step(_))
            && matches!(
                state.entries.get(i + 1),
                Some(crate::ui::chat::Entry::Step(_))
            );
        pane.push(state.drawings.rows(
            state.entries.rev(i),
            &state.entries[i],
            width,
            state.unfolded.contains(&i),
            state.at_entry == Some(i),
            joined,
        ));
    }
    state.drawings.forget_gone(&state.entries);
    // Wrapped above rather than by the widget. The program counted logical lines
    // while the terminal showed rows, so scrolling was off by however much had
    // wrapped and nothing could say which line sits at a given row - which is what
    // pointing at text with the mouse needs to know.
    state.pane = pane_area;
    let inner_h = pane_area.height as usize;
    let total = pane.len();
    // Remembered for the scroll keys, which cannot know either of these.
    state.content_lines = total;
    state.view_height = inner_h;
    // Scrolling past the top used to blank the pane: the window slid off the end of
    // the content instead of stopping at its oldest line.
    let scroll = (state.scroll as usize).min(total.saturating_sub(inner_h));
    state.scroll = scroll as u16;
    let end = total.saturating_sub(scroll);
    let start = end.saturating_sub(inner_h);
    let chosen = state
        .selecting
        .map(|(a, b)| if a <= b { (a, b) } else { (b, a) });
    let visible: Vec<Line<'static>> = pane
        .rows(start, end)
        .enumerate()
        .map(|(row, line)| {
            let row = start + row;
            match chosen {
                Some((from, to)) if row >= from.line && row <= to.line => {
                    let a = if row == from.line { from.column } else { 0 };
                    let b = if row == to.line {
                        to.column
                    } else {
                        crate::ui::chat::line_width(line)
                    };
                    crate::ui::chat::mark_selected(line, a, b)
                }
                _ => line.clone(),
            }
        })
        // What was searched for, picked out where it is. Pointing at the entry
        // left the word itself to be found by eye, which on a screenful of
        // output is the part that takes the time.
        .map(|line| match state.finding.as_ref() {
            Some((needle, _)) => crate::ui::chat::mark_found(&line, needle),
            None => line,
        })
        .collect();
    f.render_widget(Paragraph::new(visible), pane_area);
    // Nothing used to say the conversation had been scrolled back, or by how
    // much: a pane showing an hour-old answer looks exactly like a pane showing
    // the newest one.
    // Shown once the conversation has been scrolled back, where it says how far: at the
    // newest line there is nothing for it to say.
    if total > inner_h && scroll > 0 {
        let mut bar = ratatui::widgets::ScrollbarState::new(total.saturating_sub(inner_h))
            .position(total.saturating_sub(inner_h).saturating_sub(scroll));
        f.render_stateful_widget(
            ratatui::widgets::Scrollbar::new(ratatui::widgets::ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(None)
                .thumb_symbol("▐")
                .thumb_style(theme().dim),
            pane_area,
            &mut bar,
        );
    }
    // Kept for the mouse, which can only say which row was pointed at.
    state.wrapped = pane;

    draw_header(f, header, state, root);
    // The line being typed sits on a surface of its own, where the terminal can show one.
    if let Some(tint) = theme().tint {
        f.render_widget(
            ratatui::widgets::Block::default().style(Style::default().bg(tint)),
            chunks[2],
        );
    }

    // The prompt mark, in its own column, so the text the user types starts where
    // the answers do and a wrapped line does not sit under the mark.
    let typing = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(2), Constraint::Min(1)])
        .split(chunks[2]);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "❯ ",
            Style::default()
                .fg(if state.busy {
                    theme().faint()
                } else {
                    theme().accent
                })
                .add_modifier(Modifier::BOLD),
        ))),
        typing[0],
    );
    f.render_widget(input, typing[1]);
    if let Some(area) = rail_area {
        crate::ui::rail::Rail {
            model: &state.model,
            tasks: &state.tasks,
            changes: &state.changes,
            window: &state.window,
            rates: &state.rates,
            turns: &state.turns,
            engine: &state.engine,
            power: state.power.as_ref(),
            watts: &state.watts,
        }
        .render(f, area);
    }

    // The completion list, as wide as the conversation it opens over and drawn after the
    // rail, so the rail never cuts into it.
    if let Some(picker) = state.picker.as_ref() {
        picker.render(
            f,
            ratatui::layout::Rect {
                width: pane_area.width,
                ..chunks[2]
            },
        );
    }

    let waiting = state.pending_approval.is_some();
    let status = Status {
        planning: state.planning,
        pointing: state.at_entry.is_some(),
        context_percent: state.window.percent,
        // What the rail would say of the plan, when the rail is not there to say it.
        plan: match (rail_area.is_some(), state.tasks.is_empty()) {
            (false, false) => Some((
                state
                    .tasks
                    .iter()
                    .filter(|t| t.state == crate::tools::plan::State::Done)
                    .count(),
                state.tasks.len(),
            )),
            _ => None,
        },
        rail_offered: !wide,
        on_receipt: state.receipt_here().is_some(),
        attaching: {
            let line = input.lines().join("\n");
            match line.contains('@') {
                true => crate::agent::mentions::weigh(&line, root),
                false => None,
            }
        },
        step: state.step,
        max_steps,
        busy: state.busy,
        typing: !input.is_empty(),
        accepting_writes: state.accepting_writes,
        selected: state.selecting.is_some(),
        mouse_ours: state.mouse_ours,
        queued: state.queued.len(),
        rate: state.rate,
        background: state
            .background
            .as_ref()
            .map(|(what, since)| (what.clone(), since.elapsed())),
        finding: state
            .finding
            .as_ref()
            .map(|(needle, nth)| match state.found {
                0 if needle.is_empty() => "find:".to_string(),
                0 => format!("find: {needle}  nothing"),
                n => format!("find: {needle}  {} of {n}", nth % n + 1),
            }),
        said: state
            .said
            .as_ref()
            .filter(|(_, at)| at.elapsed() < SAID_FOR)
            .map(|(text, _)| text.clone()),
        phase: if waiting {
            Some("waiting for your answer".to_string())
        } else {
            match (&state.phase, state.prefill) {
                (Some(crate::agent::agent_loop::Phase::Waiting), Some((done, total))) => {
                    let cells = 10u64;
                    let full = (done * cells).checked_div(total).unwrap_or(0).min(cells) as usize;
                    Some(format!(
                        "reading the prompt {}{} {} of {}",
                        "▰".repeat(full),
                        "▱".repeat(cells as usize - full),
                        crate::ui::chat::tokens(done),
                        crate::ui::chat::tokens(total)
                    ))
                }
                (phase, _) => phase.as_ref().map(|p| p.label()),
            }
        },
        in_phase: if waiting {
            state.waiting_since.map(|t| t.elapsed()).unwrap_or_default()
        } else {
            state.phase_since.map(|t| t.elapsed()).unwrap_or_default()
        },
        tick: state.tick,
    };
    f.render_widget(
        Paragraph::new(status.to_line()).style(Style::default().fg(theme().text)),
        chunks[3],
    );
    f.render_widget(
        Paragraph::new(status.keys_within(chunks[4].width as usize)),
        chunks[4],
    );

    if let Some(sheet) = state.sheet.as_mut() {
        sheet.render(f, chunks[0]);
    }
    // Last, so it is over everything: a question being asked of the person is not
    // something anything else may cover.
    if let Some(chooser) = state.choosing.as_ref() {
        chooser.render(f, chunks[0]);
    }
}

/// The band at the top: what this is, where, on which branch with how many files changed,
/// and which model on which backend answers.
fn draw_header(f: &mut Frame, area: ratatui::layout::Rect, state: &UiState, root: &Path) {
    let t = theme();
    let surface = match t.tint {
        Some(tint) => Style::default().bg(tint),
        None => Style::default(),
    };
    let home = std::env::var("HOME").unwrap_or_default();
    let place = root.display().to_string();
    let place = match (!home.is_empty())
        .then(|| place.strip_prefix(&home))
        .flatten()
    {
        Some(rest) => format!("~{rest}"),
        None => place,
    };
    let mut left = vec![
        Span::styled(
            " lokode ",
            Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {place}"), t.dim),
    ];
    if let Some((branch, dirty)) = &state.git {
        left.push(Span::styled(
            format!("  {branch}"),
            Style::default().fg(t.syntax.keyword.fg.unwrap_or(t.accent)),
        ));
        if *dirty > 0 {
            left.push(Span::styled(
                format!(" ●{dirty}"),
                Style::default().fg(t.warn),
            ));
        }
    }
    let backend = state
        .backend
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .to_string();
    let right = vec![
        Span::styled(
            state.model.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  {backend}  {} ", state.protocol.label()), t.dim),
    ];
    let wide = |s: &[Span<'static>]| s.iter().map(|x| x.content.chars().count()).sum::<usize>();
    let room = area.width as usize;
    let mut spans = left;
    let gap = room.saturating_sub(wide(&spans) + wide(&right));
    if gap >= 2 {
        spans.push(Span::raw(" ".repeat(gap)));
        spans.extend(right);
    }
    f.render_widget(Paragraph::new(Line::from(spans)).style(surface), area);
}

/// The widest the conversation is laid out, in columns.
const READING_AT_MOST: usize = 110;

/// How tall the input may grow before it scrolls inside itself.
const INPUT_MAX_LINES: usize = 6;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::agent_loop::Origin;
    use crate::ui::chat::{Entry, Outcome, Step};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// A session in the middle of a turn: a question, a step, an answer with code, a change
    /// waiting to be agreed to, and an error.
    fn a_session() -> UiState {
        let mut state = UiState {
            model: "qwen3-coder".into(),
            mouse_ours: true,
            window: crate::agent::session::Occupancy {
                percent: 23,
                used: 2300,
                budget: 10_000,
                parts: vec![
                    ("instructions", 1000),
                    ("you", 300),
                    ("model", 400),
                    ("tools", 600),
                ],
            },
            ..Default::default()
        };
        state.backend = "http://127.0.0.1:11435".into();
        state.git = Some(("main".into(), 2));
        state.tasks = vec![
            crate::tools::plan::Task {
                what: "read the MCP client".into(),
                state: crate::tools::plan::State::Done,
            },
            crate::tools::plan::Task {
                what: "a timeout for every server".into(),
                state: crate::tools::plan::State::Doing,
            },
            crate::tools::plan::Task {
                what: "test a silent server".into(),
                state: crate::tools::plan::State::Todo,
            },
        ];
        state.changes = vec![
            crate::ui::rail::FileChange {
                path: "src/mcp/client.rs".into(),
                added: 3,
                removed: 1,
            },
            crate::ui::rail::FileChange {
                path: "src/config.rs".into(),
                added: 2,
                removed: 0,
            },
        ];
        state.turns = vec![crate::checkpoint::Turn {
            turn: 1,
            files: vec!["src/config.rs".into()],
        }];
        for rate in [38.0, 61.0, 88.0, 94.0, 90.0, 93.0] {
            state.note_rate(rate);
        }
        for watts in [120.0, 260.0, 305.0, 312.0] {
            state.note_power(crate::backend::devices::Power {
                watts,
                limit: Some(450.0),
                busy_percent: Some(87),
                devices: 1,
            });
        }
        state.engine = crate::agent::agent_loop::Engine {
            latest: Some(crate::backend::meter::Latest {
                purpose: crate::backend::meter::Purpose::Step,
                prompt_tokens: 6100,
                cached_tokens: 4200,
                accepted_tokens: 82,
                rejected_tokens: 18,
                loken: crate::backend::wire::LokenUsage {
                    queue_ms: Some(0),
                    prefill_ms: Some(700),
                    decode_ms: Some(3100),
                    energy: None,
                    kv: Some(crate::backend::wire::Kv {
                        used_tokens: 20_431,
                        capacity_tokens: 131_072,
                    }),
                },
            }),
            turn: crate::backend::meter::Spent {
                joules: 1116.0,
                requests: 5,
                shared: 1,
                estimated: 0,
            },
            session: crate::backend::meter::Spent {
                joules: 1512.0,
                requests: 7,
                shared: 1,
                estimated: 0,
            },
        };
        state.entries.extend([
            Entry::Notice("backend http://127.0.0.1:11435".into()),
            Entry::User("add a configurable timeout to the MCP calls".into()),
            Entry::Reasoning("the client awaits its reply without a bound".into()),
            Entry::Step(Step {
                outcome: Some(Outcome {
                    is_error: false,
                    body: "212 lines\n  1 use std::time::Duration;\n  2 use tokio::sync::oneshot;"
                        .into(),
                    took: std::time::Duration::from_millis(40),
                    origin: Origin::Guessed,
                }),
                ..Step::called(
                    "1".into(),
                    "read_file".into(),
                    r#"{"path":"src/mcp/client.rs"}"#,
                )
            }),
            Entry::Step(Step {
                outcome: Some(Outcome {
                    is_error: false,
                    body: "14 passed\ntest mcp::client::a_silent_server_times_out ... ok".into(),
                    took: std::time::Duration::from_millis(9800),
                    origin: Origin::Ran,
                }),
                ..Step::called("2".into(), "bash".into(), r#"{"cmd":"cargo test mcp"}"#)
            }),
            Entry::Assistant(
                "The client waits on `rx.await` forever. I add **timeout-secs**:\n\n\
                 - one setting per server, thirty seconds unless it says otherwise\n\
                 - a test against a server that never answers\n\n\
                 ```rust\nlet reply = timeout(self.timeout, rx).await??;\n```"
                    .into(),
            ),
            Entry::Approval {
                tool: "edit_file".into(),
                summary: "edit src/mcp/client.rs".into(),
                diff: Some(
                    "--- a/src/mcp/client.rs\n+++ b/src/mcp/client.rs\n@@ -212 +212 @@\n\
                     -    let reply = rx.await?;\n\
                     +    let reply = timeout(self.timeout, rx).await??;"
                        .into(),
                ),
                scope: "src/mcp/".into(),
            },
            Entry::Receipt(crate::agent::agent_loop::Receipt {
                turn: 2,
                took: std::time::Duration::from_secs(38),
                steps: 7,
                tokens_in: 8400,
                tokens_out: 1100,
                files: vec!["src/mcp/client.rs".into(), "src/config.rs".into()],
                energy: Some(crate::backend::meter::Spent {
                    joules: 1512.0,
                    requests: 7,
                    shared: 0,
                    estimated: 0,
                }),
            }),
            Entry::Notice("backend error: gave up after 3 attempts".into()),
        ]);
        state
    }

    /// The screen as text, one line per row, without the spaces that end a row.
    fn frame(state: &mut UiState, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("a test terminal");
        let input = TextArea::default();
        terminal
            .draw(|f| draw(f, state, &input, Path::new("/p/demo"), 16))
            .expect("a frame");
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                let row: String = (0..width).map(|x| buffer[(x, y)].symbol()).collect();
                row.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    }

    /// The frame drawn at each width matches the one recorded beside this file.
    ///
    /// `LOKODE_BLESS=1 cargo test` records the frames again after a change meant to show.
    #[test]
    fn the_screen_is_drawn_as_recorded() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ui/snapshots");
        let bless = std::env::var_os("LOKODE_BLESS").is_some();
        for (width, height) in [(80, 24), (120, 32), (160, 40)] {
            let drawn = frame(&mut a_session(), width, height);
            let path = dir.join(format!("frame-{width}x{height}.txt"));
            if bless {
                std::fs::create_dir_all(&dir).expect("the snapshot directory");
                std::fs::write(&path, &drawn).expect("a recorded frame");
                continue;
            }
            let recorded = std::fs::read_to_string(&path).unwrap_or_else(|_| {
                panic!(
                    "{} is missing: run LOKODE_BLESS=1 cargo test",
                    path.display()
                )
            });
            assert_eq!(
                drawn,
                recorded,
                "{width}x{height} differs from {}",
                path.display()
            );
        }
        readme_shows_the_screen(&frame(&mut a_session(), 120, 32), bless);
    }

    /// The README shows the 120-column screen as it is drawn, between its markers. Recording
    /// the screens again writes it there too.
    fn readme_shows_the_screen(drawn: &str, bless: bool) {
        const OPEN: &str = "<!-- screen:120x32 -->\n```text\n";
        const CLOSE: &str = "```\n<!-- /screen -->";
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md");
        let readme = std::fs::read_to_string(&path).expect("the README");
        let from = readme
            .find(OPEN)
            .expect("the README marks where the screen goes")
            + OPEN.len();
        let to = from
            + readme[from..]
                .find(CLOSE)
                .expect("the screen's closing mark");
        if bless {
            let updated = format!("{}{drawn}{}", &readme[..from], &readme[to..]);
            std::fs::write(&path, updated).expect("the README written");
            return;
        }
        assert_eq!(
            &readme[from..to],
            drawn,
            "the README shows an older screen: run LOKODE_BLESS=1 cargo test"
        );
    }

    /// A frame that follows one with nothing changed draws no entry again, however long
    /// the session, and a new entry is the only one drawn.
    #[test]
    fn a_long_session_draws_only_what_changed() {
        let mut state = a_session();
        for i in 0..500 {
            state.entries.extend([
                Entry::Step(Step {
                    outcome: Some(Outcome {
                        is_error: false,
                        body: format!("{i} match(es)\nsrc/lib.rs:{i}: step {i}"),
                        took: std::time::Duration::ZERO,
                        origin: Origin::Ran,
                    }),
                    ..Step::called(
                        i.to_string(),
                        "grep".into(),
                        &format!(r#"{{"pattern":"step {i}"}}"#),
                    )
                }),
                Entry::Assistant(format!("step {i} done")),
            ]);
        }
        let all = state.entries.len();
        frame(&mut state, 120, 32);
        assert_eq!(state.drawings.drawn, all);
        frame(&mut state, 120, 32);
        assert_eq!(
            state.drawings.drawn, all,
            "an unchanged frame drew entries again"
        );
        state.entries.push(Entry::Notice("one more".into()));
        frame(&mut state, 120, 32);
        assert_eq!(state.drawings.drawn, all + 1);
    }

    /// One scene for the screens: a name, a size, the state and what is typed.
    struct Scene {
        name: &'static str,
        width: u16,
        height: u16,
        state: UiState,
        typed: &'static str,
    }

    /// The scenes a design pass looks at: a session at three widths, a turn at work, the
    /// completion lists, a list a command opens, and two pages.
    fn scenes() -> Vec<Scene> {
        use crate::ui::picker::{Choice, Kind, Picker};
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut out = Vec::new();
        for (name, width, height) in [
            ("session-160", 160, 42),
            ("session-120", 120, 34),
            ("session-90", 90, 30),
        ] {
            out.push(Scene {
                name,
                width,
                height,
                state: a_session(),
                typed: "",
            });
        }
        let mut busy = a_session();
        busy.busy = true;
        busy.tick = 3;
        busy.step = 5;
        busy.phase = Some(crate::agent::agent_loop::Phase::Waiting);
        busy.phase_since = Some(std::time::Instant::now());
        busy.prefill = Some((8192, 20431));
        let mut running = Step::called("9".into(), "bash".into(), r#"{"cmd":"cargo test"}"#);
        running.live = (1..=14)
            .map(|i| format!("test module::case_{i} ... ok"))
            .collect::<Vec<_>>()
            .join("\n");
        busy.entries.push(Entry::Step(running));
        out.push(Scene {
            name: "busy-120",
            width: 120,
            height: 34,
            state: busy,
            typed: "and add a test for a silent server",
        });
        let mut commands = a_session();
        let mut picker = Picker::new(Kind::Command, crate::ui::commands::command_choices(root));
        picker.narrow("");
        commands.picker = Some(picker);
        out.push(Scene {
            name: "complete-command-120",
            width: 120,
            height: 34,
            state: commands,
            typed: "/",
        });
        let mut paths = a_session();
        let mut picker = Picker::new(
            Kind::Path,
            [
                "src/ui/app_loop.rs",
                "src/ui/chat.rs",
                "src/ui/commands.rs",
                "src/ui/diff.rs",
                "src/ui/highlight.rs",
                "src/ui/markdown.rs",
                "src/ui/rail.rs",
            ]
            .iter()
            .map(|p| Choice::new(*p, ""))
            .collect(),
        );
        picker.narrow("src/ui/");
        paths.picker = Some(picker);
        out.push(Scene {
            name: "complete-path-120",
            width: 120,
            height: 34,
            state: paths,
            typed: "why does @src/ui/",
        });
        let mut models = a_session();
        models.choosing = Some(crate::ui::chooser::Chooser::new(
            "model",
            "which model answers",
            [
                "qwen3-coder:30b",
                "qwen3:8b",
                "deepseek-v4.1-flash",
                "gpt-oss:20b",
            ]
            .iter()
            .map(|m| {
                crate::ui::chooser::Choice::new(*m, if *m == "qwen3:8b" { "search" } else { "" })
            })
            .collect(),
            Some("qwen3-coder:30b".into()),
        ));
        out.push(Scene {
            name: "choose-model-120",
            width: 120,
            height: 34,
            state: models,
            typed: "",
        });
        let mut help = a_session();
        help.sheet = Some(crate::ui::sheet::Sheet::new(
            "help",
            &crate::ui::commands::help(root),
        ));
        out.push(Scene {
            name: "help-120",
            width: 120,
            height: 34,
            state: help,
            typed: "",
        });
        let mut context = a_session();
        context.sheet = Some(crate::ui::sheet::Sheet::with(
            "context",
            crate::ui::sheet::Body::Window(context.window.clone()),
        ));
        out.push(Scene {
            name: "context-120",
            width: 120,
            height: 34,
            state: context,
            typed: "",
        });
        out
    }

    /// A colour as the screens write it: `#rrggbb`, a named colour, or `reset`.
    fn colour_name(c: ratatui::style::Color) -> String {
        match c {
            ratatui::style::Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
            ratatui::style::Color::Reset => "reset".into(),
            other => format!("{other:?}"),
        }
    }

    /// Draw every scene and write its cells, with their colours and weights, as JSON in
    /// the directory `LOKODE_SCREENS` names, under the theme `LOKODE_SCREEN_THEME` names.
    /// Ignored unless asked for: `scripts/screens.sh` runs it and turns the cells into
    /// pictures.
    #[test]
    #[ignore]
    fn screens() {
        let Some(dir) = std::env::var_os("LOKODE_SCREENS") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        let light = std::env::var("LOKODE_SCREEN_THEME").as_deref() == Ok("light");
        let (fg, bg) = match light {
            true => ((30, 30, 30), (250, 250, 248)),
            false => ((208, 208, 208), (24, 26, 30)),
        };
        crate::ui::theme::set(crate::ui::theme::Theme::for_colours(fg, bg, true));
        std::fs::create_dir_all(&dir).expect("the screens directory");
        for mut scene in scenes() {
            let mut terminal =
                Terminal::new(TestBackend::new(scene.width, scene.height)).expect("a terminal");
            let input = crate::ui::input::fresh_input(scene.typed);
            terminal
                .draw(|f| draw(f, &mut scene.state, &input, Path::new("/p/demo"), 16))
                .expect("a frame");
            let buffer = terminal.backend().buffer();
            let mut rows = Vec::new();
            for y in 0..scene.height {
                let mut cells = Vec::new();
                for x in 0..scene.width {
                    let cell = &buffer[(x, y)];
                    let m = cell.modifier;
                    cells.push(serde_json::json!([
                        cell.symbol(),
                        colour_name(cell.fg),
                        colour_name(cell.bg),
                        m.contains(Modifier::BOLD),
                        m.contains(Modifier::ITALIC),
                        m.contains(Modifier::UNDERLINED),
                        m.contains(Modifier::REVERSED),
                        m.contains(Modifier::DIM),
                    ]));
                }
                rows.push(serde_json::Value::Array(cells));
            }
            let doc = serde_json::json!({
                "name": scene.name,
                "theme": if light { "light" } else { "dark" },
                "fg": format!("#{:02x}{:02x}{:02x}", fg.0, fg.1, fg.2),
                "bg": format!("#{:02x}{:02x}{:02x}", bg.0, bg.1, bg.2),
                "rows": rows,
            });
            let path = dir.join(format!(
                "{}-{}.json",
                scene.name,
                if light { "light" } else { "dark" }
            ));
            std::fs::write(&path, doc.to_string()).expect("a screen written");
        }
    }
}
