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
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(typed),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(f.area());

    // Wrapped entry by entry, so where each one begins is known. Pointing at a
    // step said nothing about where it is: the mark went on an entry that could be
    // a screen and a half above the pane, and the only way to see it was to guess
    // how far to scroll.
    // The last column is kept for the scrollbar, so it never sits over text.
    let width = chunks[0].width.saturating_sub(1) as usize;
    let mut pane = crate::ui::thread::Pane::default();
    for i in 0..state.entries.len() {
        pane.push(state.drawings.rows(
            state.entries.rev(i),
            &state.entries[i],
            width,
            state.unfolded.contains(&i),
            state.at_entry == Some(i),
        ));
    }
    state.drawings.forget_gone(&state.entries);
    // Wrapped above rather than by the widget. The program counted logical lines
    // while the terminal showed rows, so scrolling was off by however much had
    // wrapped and nothing could say which line sits at a given row - which is what
    // pointing at text with the mouse needs to know.
    state.pane = chunks[0];
    let inner_h = chunks[0].height as usize;
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
    f.render_widget(Paragraph::new(visible), chunks[0]);
    // Nothing used to say the conversation had been scrolled back, or by how
    // much: a pane showing an hour-old answer looks exactly like a pane showing
    // the newest one.
    if total > inner_h {
        let mut bar = ratatui::widgets::ScrollbarState::new(total.saturating_sub(inner_h))
            .position(total.saturating_sub(inner_h).saturating_sub(scroll));
        f.render_stateful_widget(
            ratatui::widgets::Scrollbar::new(ratatui::widgets::ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("▕"))
                .track_style(theme().dim)
                .thumb_symbol("▐")
                .thumb_style(Style::default().fg(theme().text)),
            chunks[0],
            &mut bar,
        );
    }
    // Kept for the mouse, which can only say which row was pointed at.
    state.wrapped = pane;

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
    if let Some(picker) = state.picker.as_ref() {
        picker.render(f, typing[1]);
    }

    let waiting = state.pending_approval.is_some();
    let status = Status {
        model: state.model.clone(),
        // The last component: it is what names the project, and the rest is where
        // it happens to sit on this disk.
        root: root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.display().to_string()),
        planning: state.planning,
        pointing: state.at_entry.is_some(),
        context_percent: state.context_percent,
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
            state.phase.as_ref().map(|p| p.label())
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

/// How tall the input may grow before it scrolls inside itself.
const INPUT_MAX_LINES: usize = 6;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::chat::Entry;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// A session in the middle of a turn: a question, a step, an answer with code, a change
    /// waiting to be agreed to, and an error.
    fn a_session() -> UiState {
        let mut state = UiState {
            model: "qwen3-coder".into(),
            mouse_ours: true,
            context_percent: 23,
            ..Default::default()
        };
        state.entries.extend([
            Entry::Notice("backend http://127.0.0.1:11435".into()),
            Entry::User("add a configurable timeout to the MCP calls".into()),
            Entry::Reasoning("the client awaits its reply without a bound".into()),
            Entry::ToolCall {
                name: "read_file".into(),
                args: r#"{"path":"src/mcp/client.rs"}"#.into(),
            },
            Entry::ToolResult {
                name: "read_file".into(),
                is_error: false,
                body: "212 lines\n  1 use std::time::Duration;\n  2 use tokio::sync::oneshot;"
                    .into(),
            },
            Entry::Assistant(
                "The client waits on `rx.await` forever. I add **timeout-secs**:\n\n\
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
    }

    /// A frame that follows one with nothing changed draws no entry again, however long
    /// the session, and a new entry is the only one drawn.
    #[test]
    fn a_long_session_draws_only_what_changed() {
        let mut state = a_session();
        for i in 0..500 {
            state.entries.extend([
                Entry::ToolCall {
                    name: "grep".into(),
                    args: format!(r#"{{"pattern":"step {i}"}}"#),
                },
                Entry::ToolResult {
                    name: "grep".into(),
                    is_error: false,
                    body: format!("{i} match(es)\nsrc/lib.rs:{i}: step {i}"),
                },
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
}
