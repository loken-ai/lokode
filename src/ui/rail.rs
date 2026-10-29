//! The rail beside the conversation: the model's plan, what the session changed, what fills
//! the window, how fast the model answers, and the turns that can be put back.
//!
//! A section with nothing to say is left out. The rail takes its own column from a wide
//! enough pane, and opens over the conversation on a narrow one when asked for.

use std::collections::VecDeque;

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

use crate::agent::agent_loop::Engine;
use crate::agent::session::Occupancy;
use crate::checkpoint::Turn;
use crate::tools::plan::{State, Task};
use crate::ui::theme::theme;

/// The narrowest pane that gives the rail a column of its own.
pub const WIDE_FROM: u16 = 120;

/// How wide the rail is.
pub const WIDTH: u16 = 36;

/// How many speeds the sparkline keeps.
pub const RATES_KEPT: usize = 24;

/// How many turns that can be put back are listed.
/// For each of the rail's first rows, the running thing on it, laid out as the rail lays
/// out its section of running things.
pub fn running_rows(jobs: &[crate::ui::state::Job]) -> Vec<Option<usize>> {
    if jobs.is_empty() {
        return Vec::new();
    }
    let mut rows = vec![None, None];
    for (i, job) in jobs.iter().enumerate().take(JOBS_SHOWN) {
        rows.push(Some(i));
        if job.doing.is_some() {
            rows.push(Some(i));
        }
    }
    rows
}

/// How many running things the rail lists before it counts the rest.
const JOBS_SHOWN: usize = 5;

const TURNS_SHOWN: usize = 5;

/// How many changed files are listed before the rest are counted.
const FILES_SHOWN: usize = 6;

/// The heights of a sparkline's bars, lowest first.
const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// One file the session changed, with the lines it added and removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    pub added: usize,
    pub removed: usize,
}

/// What the rail draws.
pub struct Rail<'a> {
    pub model: &'a str,
    pub tasks: &'a [Task],
    pub changes: &'a [FileChange],
    pub window: &'a Occupancy,
    pub rates: &'a VecDeque<f64>,
    pub turns: &'a [Turn],
    pub engine: &'a Engine,
    pub power: Option<&'a crate::backend::devices::Power>,
    pub watts: &'a VecDeque<f64>,
    /// What is running now.
    pub jobs: &'a [crate::ui::state::Job],
}

/// The colours of the window's parts, in the order the session measures them.
pub(crate) fn part_colour(at: usize) -> Color {
    let t = theme();
    match at {
        0 => t.accent,
        1 => t.syntax.function.fg.unwrap_or(t.accent),
        2 => t.syntax.keyword.fg.unwrap_or(t.tool),
        _ => t.tool,
    }
}

impl Rail<'_> {
    /// The rail's rows, `width` columns wide.
    pub fn lines(&self, width: usize) -> Vec<Line<'static>> {
        let t = theme();
        let inner = width.saturating_sub(2);
        let mut out: Vec<Line<'static>> = vec![Line::from("")];
        let heading = |out: &mut Vec<Line<'static>>, name: &str, right: String| {
            let style = t.dim.add_modifier(Modifier::BOLD);
            let gap = inner.saturating_sub(name.chars().count() + right.chars().count());
            out.push(Line::from(vec![
                Span::raw(" "),
                Span::styled(name.to_string(), style),
                Span::raw(" ".repeat(gap.max(1))),
                Span::styled(right, t.dim),
            ]));
        };

        // First while there is any: what is running is what can still change, or be stopped.
        if !self.jobs.is_empty() {
            heading(&mut out, "RUNNING", self.jobs.len().to_string());
            for job in self.jobs.iter().take(JOBS_SHOWN) {
                let took = match job.took >= crate::ui::chat::SHOWN_FROM {
                    true => crate::ui::chat::how_long(job.took),
                    false => String::new(),
                };
                let room = inner.saturating_sub(3 + took.chars().count() + 1);
                let what = clip(&job.what, room);
                let gap = room.saturating_sub(what.chars().count()) + 1;
                out.push(Line::from(vec![
                    Span::raw(" "),
                    Span::styled("◌", Style::default().fg(t.accent)),
                    Span::raw(" "),
                    Span::styled(what, Style::default().fg(t.tool)),
                    Span::raw(" ".repeat(gap)),
                    Span::styled(took, t.dim),
                ]));
                if let Some(doing) = &job.doing {
                    out.push(Line::from(Span::styled(
                        format!("   › {}", clip(doing, inner.saturating_sub(5))),
                        t.dim,
                    )));
                }
            }
            if self.jobs.len() > JOBS_SHOWN {
                out.push(Line::from(Span::styled(
                    format!(" and {} more, /jobs", self.jobs.len() - JOBS_SHOWN),
                    t.dim,
                )));
            }
            out.push(Line::from(""));
        }

        if !self.tasks.is_empty() {
            let done = self.tasks.iter().filter(|x| x.state == State::Done).count();
            heading(&mut out, "PLAN", format!("{done}/{}", self.tasks.len()));
            for task in self.tasks {
                let (mark, style, text) = match task.state {
                    State::Done => ("●", Style::default().fg(t.good), t.dim),
                    State::Doing => ("◉", Style::default().fg(t.warn), Style::default()),
                    State::Todo => ("○", t.dim, Style::default()),
                };
                out.push(Line::from(vec![
                    Span::raw(" "),
                    Span::styled(mark, style),
                    Span::styled(format!(" {}", clip(&task.what, inner - 2)), text),
                ]));
            }
            out.push(Line::from(""));
        }

        if !self.changes.is_empty() {
            let added: usize = self.changes.iter().map(|c| c.added).sum();
            let removed: usize = self.changes.iter().map(|c| c.removed).sum();
            heading(&mut out, "CHANGES", format!("+{added} -{removed}"));
            for c in self.changes.iter().take(FILES_SHOWN) {
                let counts = format!("+{} -{}", c.added, c.removed);
                let room = inner.saturating_sub(counts.chars().count() + 1);
                let path = clip_left(&c.path, room);
                let gap = inner.saturating_sub(path.chars().count() + counts.chars().count());
                out.push(Line::from(vec![
                    Span::raw(" "),
                    Span::raw(path),
                    Span::raw(" ".repeat(gap.max(1))),
                    Span::styled(format!("+{}", c.added), Style::default().fg(t.good)),
                    Span::styled(format!(" -{}", c.removed), Style::default().fg(t.bad)),
                ]));
            }
            if self.changes.len() > FILES_SHOWN {
                out.push(Line::from(Span::styled(
                    format!(" and {} more", self.changes.len() - FILES_SHOWN),
                    t.dim,
                )));
            }
            out.push(Line::from(""));
        }

        if self.window.budget > 0 {
            let percent = self.window.percent;
            let pct_style = match percent {
                0..=74 => Style::default(),
                75..=99 => Style::default().fg(t.warn),
                _ => Style::default().fg(t.bad),
            };
            heading(&mut out, "CONTEXT", format!("{percent}%"));
            if let Some(span) = out.last_mut().and_then(|l| l.spans.last_mut()) {
                span.style = pct_style;
            }
            out.push(self.window_bar(inner));
            let shares: Vec<(usize, &str, usize)> = self
                .window
                .parts
                .iter()
                .enumerate()
                .filter(|(_, (_, chars))| *chars > 0)
                .map(|(at, (name, chars))| (at, *name, chars * 100 / self.window.budget.max(1)))
                .collect();
            for pair in shares.chunks(2) {
                let mut spans = vec![Span::raw(" ")];
                for (at, name, share) in pair {
                    spans.push(Span::styled("█", Style::default().fg(part_colour(*at))));
                    spans.push(Span::styled(format!(" {name} {share}%  "), t.dim));
                }
                out.push(Line::from(spans));
            }
            out.push(Line::from(""));
        }

        heading(&mut out, "ENGINE", String::new());
        out.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                clip(self.model, inner),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]));
        if let Some(last) = self.rates.back() {
            let room = inner.saturating_sub(12).min(self.rates.len());
            out.push(Line::from(vec![
                Span::raw(" "),
                Span::styled(sparkline(self.rates, room), Style::default().fg(t.accent)),
                Span::raw(" "),
                Span::styled(
                    format!("{last:.0}"),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(" tok/s", t.dim),
            ]));
        }
        out.extend(self.engine_lines(inner));
        out.push(Line::from(""));

        if !self.turns.is_empty() {
            heading(&mut out, "TURNS TO PUT BACK", String::new());
            for turn in self.turns.iter().take(TURNS_SHOWN) {
                let files = match turn.files.len() {
                    1 => clip_left(&turn.files[0], inner.saturating_sub(12)),
                    n => format!("{n} files"),
                };
                out.push(Line::from(vec![
                    Span::raw(" "),
                    Span::styled(format!("{:>3}", turn.turn), t.key),
                    Span::styled(format!("  {files}"), t.dim),
                ]));
            }
            out.push(Line::from(Span::styled(
                " /undo <turn> puts one back",
                t.dim,
            )));
        }
        out
    }

    /// What the backend measured: how long the latest request waited, read and wrote, how
    /// full its cache is, how much of a draft it kept, and the energy of the turn and the
    /// session. Each line only when the backend said what it needs.
    fn engine_lines(&self, inner: usize) -> Vec<Line<'static>> {
        let t = theme();
        let mut out = Vec::new();
        let label = |s: &str| Span::styled(format!(" {s:<8}"), t.dim);
        if let Some(power) = self.power {
            let mut spans = vec![label("power")];
            let room = inner.saturating_sub(24).min(self.watts.len());
            if room > 1 {
                spans.push(Span::styled(
                    sparkline(self.watts, room),
                    Style::default().fg(t.warn),
                ));
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled(
                format!("{:.0} W", power.watts),
                Style::default().add_modifier(Modifier::BOLD),
            ));
            if let Some(busy) = power.busy_percent {
                spans.push(Span::styled(format!("  {busy}% busy"), t.dim));
            }
            out.push(Line::from(spans));
        }
        if let Some(latest) = &self.engine.latest {
            let l = &latest.loken;
            let ms = |v: u64| crate::ui::chat::how_long(std::time::Duration::from_millis(v));
            let mut times = Vec::new();
            if let Some(q) = l.queue_ms.filter(|q| *q > 0) {
                times.push(format!("wait {}", ms(q)));
            }
            if let Some(p) = l.prefill_ms {
                times.push(format!("read {}", ms(p)));
            }
            if let Some(d) = l.decode_ms {
                times.push(format!("write {}", ms(d)));
            }
            if !times.is_empty() {
                out.push(Line::from(vec![
                    label("time"),
                    Span::raw(clip(&times.join(" · "), inner.saturating_sub(9))),
                ]));
            }
            if latest.prompt_tokens > 0 {
                let cached = match latest.cached_tokens {
                    0 => String::new(),
                    c => format!(", {} held", crate::ui::chat::tokens(c as u64)),
                };
                out.push(Line::from(vec![
                    label("prompt"),
                    Span::raw(format!(
                        "{}{cached}",
                        crate::ui::chat::tokens(latest.prompt_tokens as u64)
                    )),
                ]));
            }
            if let Some(kv) = l.kv.filter(|kv| kv.capacity_tokens > 0) {
                let cells = 10usize;
                let full = ((kv.used_tokens * cells as u64 + kv.capacity_tokens / 2)
                    / kv.capacity_tokens)
                    .min(cells as u64) as usize;
                out.push(Line::from(vec![
                    label("cache"),
                    Span::styled("▰".repeat(full), Style::default().fg(t.accent)),
                    Span::styled("▱".repeat(cells - full), t.dim),
                    Span::raw(format!(" {}%", kv.used_tokens * 100 / kv.capacity_tokens)),
                ]));
            }
            let drafted = latest.accepted_tokens + latest.rejected_tokens;
            if let Some(kept) = (latest.accepted_tokens * 100).checked_div(drafted) {
                out.push(Line::from(vec![
                    label("drafts"),
                    Span::raw(format!("{kept}% kept")),
                ]));
            }
        }
        if self.engine.session.is_measured() {
            out.push(Line::from(vec![
                label("energy"),
                Span::styled(
                    self.engine.turn.describe(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  session {}", self.engine.session.describe()),
                    t.dim,
                ),
            ]));
        }
        out
    }

    /// The window as a bar, `cells` wide.
    fn window_bar(&self, cells: usize) -> Line<'static> {
        window_bar(self.window, cells)
    }

    /// Draw the rail into `area`, on the raised surface when the terminal shows one.
    pub fn render(&self, f: &mut Frame, area: Rect) {
        let surface = match theme().tint {
            Some(tint) => Style::default().bg(tint),
            None => Style::default(),
        };
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(self.lines(area.width as usize)).style(surface),
            area,
        );
    }
}

/// The window as a bar: each part in its colour, by its share of the budget, then what is
/// free.
pub(crate) fn window_bar(window: &Occupancy, cells: usize) -> Line<'static> {
    let budget = window.budget.max(1);
    let mut spans = vec![Span::raw(" ")];
    // Each part ends where the running total of the parts falls on the bar, so rounding
    // never loses or adds a cell.
    let mut used = 0usize;
    let mut total = 0usize;
    for (at, (_, chars)) in window.parts.iter().enumerate() {
        total += chars;
        let end = ((total * cells + budget / 2) / budget).min(cells);
        let n = end.saturating_sub(used);
        if n > 0 {
            spans.push(Span::styled(
                "█".repeat(n),
                Style::default().fg(part_colour(at)),
            ));
            used += n;
        }
    }
    if used < cells {
        spans.push(Span::styled("·".repeat(cells - used), theme().dim));
    }
    Line::from(spans)
}

/// The last `cells` speeds as bars scaled to the fastest of them.
fn sparkline(rates: &VecDeque<f64>, cells: usize) -> String {
    let shown: Vec<f64> = rates.iter().rev().take(cells).rev().copied().collect();
    let top = shown.iter().copied().fold(0.0f64, f64::max);
    shown
        .iter()
        .map(|r| match top > 0.0 {
            true => BARS[((r / top) * (BARS.len() - 1) as f64).round() as usize],
            false => BARS[0],
        })
        .collect()
}

/// `s` cut to `n` characters, ending in an ellipsis when it was cut.
fn clip(s: &str, n: usize) -> String {
    match s.chars().count() > n {
        true => s.chars().take(n.saturating_sub(1)).chain(['…']).collect(),
        false => s.to_string(),
    }
}

/// `s` cut to `n` characters from the left, so the end of a path stays.
fn clip_left(s: &str, n: usize) -> String {
    let count = s.chars().count();
    match count > n {
        true => std::iter::once('…')
            .chain(s.chars().skip(count + 1 - n.max(1)))
            .collect(),
        false => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::chat::line_text;

    fn rail_text(rail: &Rail<'_>) -> Vec<String> {
        rail.lines(WIDTH as usize).iter().map(line_text).collect()
    }

    fn occupancy() -> Occupancy {
        Occupancy {
            percent: 23,
            used: 2300,
            budget: 10_000,
            parts: vec![
                ("instructions", 1000),
                ("you", 300),
                ("model", 400),
                ("tools", 600),
            ],
        }
    }

    /// A section with nothing in it is left out, and the engine is always there.
    /// What runs heads the rail, each with how long it has run and, for a delegated
    /// session, the call it is on.
    #[test]
    fn what_runs_heads_the_rail() {
        let rates = VecDeque::new();
        let jobs = [
            crate::ui::state::Job {
                what: "delegate  find every timeout".into(),
                took: std::time::Duration::from_secs(12),
                entry: Some(3),
                doing: Some("read_file  src/config.rs".into()),
                aside: None,
                stoppable: true,
            },
            crate::ui::state::Job {
                what: "$  cargo build".into(),
                took: std::time::Duration::from_secs(75),
                entry: Some(5),
                doing: None,
                aside: None,
                stoppable: true,
            },
        ];
        let rail = Rail {
            model: "qwen3-coder",
            tasks: &[],
            changes: &[],
            window: &Occupancy::default(),
            rates: &rates,
            turns: &[],
            engine: &Engine::default(),
            power: None,
            watts: &VecDeque::new(),
            jobs: &jobs,
        };
        let text = rail_text(&rail);
        assert!(
            text[1].starts_with(" RUNNING") && text[1].trim_end().ends_with('2'),
            "{text:?}"
        );
        assert!(text[2].starts_with(" ◌ delegate  find every"), "{text:?}");
        assert!(text[2].trim_end().ends_with("12.0s"), "{text:?}");
        assert_eq!(text[3].trim_end(), "   › read_file  src/config.rs");
        assert!(text[4].trim_end().ends_with("1m15s"), "{text:?}");
        // A click finds the thing on the row it lands on.
        assert_eq!(running_rows(&jobs), [None, None, Some(0), Some(0), Some(1)]);
    }

    #[test]
    fn an_empty_session_shows_only_the_engine() {
        let rates = VecDeque::new();
        let rail = Rail {
            model: "qwen3-coder",
            tasks: &[],
            changes: &[],
            window: &Occupancy::default(),
            rates: &rates,
            turns: &[],
            engine: &Engine::default(),
            power: None,
            watts: &VecDeque::new(),
            jobs: &[],
        };
        let text = rail_text(&rail);
        assert!(text.iter().any(|l| l.starts_with(" ENGINE")));
        assert!(!text
            .iter()
            .any(|l| l.contains("PLAN") || l.contains("CHANGES")));
    }

    /// The plan counts what is done, and every row fits the rail.
    #[test]
    fn the_plan_counts_what_is_done_and_fits() {
        let tasks = vec![
            Task {
                what: "read the client".into(),
                state: State::Done,
            },
            Task {
                what: "a timeout for every server, however slowly it answers".into(),
                state: State::Doing,
            },
            Task {
                what: "test it".into(),
                state: State::Todo,
            },
        ];
        let changes = vec![FileChange {
            path: "src/some/deeply/nested/module/client.rs".into(),
            added: 9,
            removed: 2,
        }];
        let mut rates = VecDeque::new();
        rates.extend([40.0, 80.0, 94.0]);
        let turns = vec![Turn {
            turn: 3,
            files: vec!["src/config.rs".into()],
        }];
        let window = occupancy();
        let rail = Rail {
            model: "qwen3-coder",
            tasks: &tasks,
            changes: &changes,
            window: &window,
            rates: &rates,
            turns: &turns,
            engine: &Engine::default(),
            power: None,
            watts: &VecDeque::new(),
            jobs: &[],
        };
        let text = rail_text(&rail);
        assert!(
            text.iter()
                .any(|l| l.starts_with(" PLAN") && l.ends_with("1/3")),
            "{text:?}"
        );
        assert!(
            text.iter().any(|l| l.ends_with("client.rs +9 -2")),
            "{text:?}"
        );
        assert!(text.iter().any(|l| l.contains("94 tok/s")), "{text:?}");
        assert!(
            text.iter().any(|l| l.contains("  3  src/config.rs")),
            "{text:?}"
        );
        for l in &text {
            assert!(
                l.chars().count() <= WIDTH as usize,
                "{l:?} is wider than the rail"
            );
        }
    }

    /// The bar gives each part its share of the budget and leaves the rest free.
    #[test]
    fn the_window_bar_is_drawn_to_scale() {
        let rates = VecDeque::new();
        let window = occupancy();
        let rail = Rail {
            model: "m",
            tasks: &[],
            changes: &[],
            window: &window,
            rates: &rates,
            turns: &[],
            engine: &Engine::default(),
            power: None,
            watts: &VecDeque::new(),
            jobs: &[],
        };
        let bar = line_text(&rail.window_bar(20));
        assert_eq!(bar.chars().filter(|&c| c == '█').count(), 5);
        assert_eq!(bar.chars().filter(|&c| c == '·').count(), 15);
    }

    #[test]
    fn a_sparkline_is_scaled_to_its_fastest() {
        let rates: VecDeque<f64> = [10.0, 50.0, 100.0].into_iter().collect();
        assert_eq!(sparkline(&rates, 3), "▂▅█");
        assert_eq!(sparkline(&rates, 2), "▅█");
    }

    #[test]
    fn a_long_path_keeps_its_end() {
        assert_eq!(clip_left("src/ui/chat.rs", 10), "…i/chat.rs");
        assert_eq!(clip_left("a.rs", 10), "a.rs");
    }
}
