//! A list to choose from, opened by a command.
//!
//! A command hands over what there is to choose from.
//! It also hands over the name of the command to run with what is chosen.
//!
//! The list filters as it is typed into.
//! The arrows move.
//! Enter picks.
//! Escape leaves everything as it was.

use ratatui::layout::Rect;
use ratatui::Frame;

pub use crate::ui::picker::Choice;

/// How many rows of the list are shown at once.
const ROWS: usize = 12;

/// A list over the conversation, and what to do with what is taken from it.
pub struct Chooser {
    /// The command run with what is chosen, as it would have been typed.
    verb: String,
    /// What is being chosen, in a word or two.
    title: String,
    all: Vec<Choice>,
    /// What is in force now: marked in the list, and where the highlight starts.
    marked: Option<String>,
    /// What has been typed to narrow the list.
    typed: String,
    /// Where the highlight is, among what the filter leaves.
    at: usize,
}

impl Chooser {
    pub fn new(
        verb: impl Into<String>,
        title: impl Into<String>,
        all: Vec<Choice>,
        marked: Option<String>,
    ) -> Self {
        let mut out = Self {
            verb: verb.into(),
            title: title.into(),
            all,
            marked,
            typed: String::new(),
            at: 0,
        };
        // Open on what is in force.
        if let Some(marked) = out.marked.clone() {
            if let Some(i) = out.shown().iter().position(|c| c.value == marked) {
                out.at = i;
            }
        }
        out
    }

    /// What is being chosen, in a word or two.
    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn verb(&self) -> &str {
        &self.verb
    }

    /// What the filter leaves, what starts with it first.
    pub fn shown(&self) -> Vec<&Choice> {
        if self.typed.is_empty() {
            return self.all.iter().collect();
        }
        let typed = self.typed.to_lowercase();
        let mut starts: Vec<&Choice> = Vec::new();
        let mut contains: Vec<&Choice> = Vec::new();
        for c in &self.all {
            let value = c.value.to_lowercase();
            if value.starts_with(&typed) {
                starts.push(c);
            } else if value.contains(&typed) {
                contains.push(c);
            }
        }
        starts.extend(contains);
        starts
    }

    /// Narrow by one more character.
    pub fn push(&mut self, c: char) {
        self.typed.push(c);
        self.at = 0;
    }

    /// Take back the last character of the filter.
    pub fn pop(&mut self) {
        self.typed.pop();
        self.at = 0;
    }

    pub fn move_by(&mut self, delta: isize) {
        let n = self.shown().len();
        if n == 0 {
            return;
        }
        self.at = (self.at as isize + delta).rem_euclid(n as isize) as usize;
    }

    /// What is highlighted now, or nothing when the filter leaves nothing.
    /// Highlight the item at `at` among what the filter leaves.
    pub fn set_at(&mut self, at: usize) {
        self.at = at.min(self.shown().len().saturating_sub(1));
    }

    pub fn chosen(&self) -> Option<String> {
        self.shown().get(self.at).map(|c| c.value.clone())
    }

    /// The rows to show, and where the highlight sits among them.
    ///
    /// The window follows the highlight, not the top of the list.
    /// The names shown change as the highlight moves down.
    fn window(&self) -> (usize, usize) {
        let shown = self.shown().len();
        let rows = shown.min(ROWS);
        let first = self
            .at
            .saturating_sub(rows / 2)
            .min(shown.saturating_sub(rows));
        (first, rows)
    }

    /// Draw it over `area`, filling it.
    /// Draw it at the foot of `area`.
    /// Return the card's area and the first item on screen, for the mouse.
    pub fn render(&self, f: &mut Frame, area: Rect) -> (Rect, usize) {
        let shown = self.shown();
        let (first, rows) = self.window();
        let height = (rows.max(1) as u16 + 1).min(area.height);
        let area = Rect {
            x: area.x,
            y: area.y + area.height.saturating_sub(height),
            width: area.width,
            height,
        };
        crate::ui::listing::Listing {
            title: &self.title,
            items: &shown,
            at: self.at,
            first,
            rows,
            filter: &self.typed,
            marked: self.marked.as_deref(),
            keys: &[("enter", "choose"), ("esc", "cancel")],
            paths: false,
        }
        .render(f, area);
        (area, first)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn models() -> Chooser {
        Chooser::new(
            "model",
            "model",
            vec![
                Choice::new("qwen3-coder:30b", ""),
                Choice::new("qwen3:8b", "plans"),
                Choice::new("devstral:24b", ""),
            ],
            Some("qwen3:8b".into()),
        )
    }

    /// A list opens on the item in force, not on the first name.
    #[test]
    fn the_list_opens_on_what_is_in_force() {
        assert_eq!(models().chosen().as_deref(), Some("qwen3:8b"));
    }

    /// Typing narrows the list.
    /// What starts with the letters comes before what merely holds them.
    #[test]
    fn typing_narrows_to_what_was_meant() {
        let mut c = models();
        for ch in "qwen3:".chars() {
            c.push(ch);
        }
        let shown: Vec<&str> = c.shown().iter().map(|c| c.value.as_str()).collect();
        assert_eq!(shown, vec!["qwen3:8b"]);
        assert_eq!(c.chosen().as_deref(), Some("qwen3:8b"));
        c.pop();
        assert_eq!(c.shown().len(), 2, "qwen3-coder holds the letters too");
    }

    /// The highlight wraps rather than sticking at either end.
    #[test]
    fn the_arrows_go_round() {
        let mut c = models();
        c.move_by(-1);
        assert_eq!(c.chosen().as_deref(), Some("qwen3-coder:30b"));
        c.move_by(-1);
        assert_eq!(c.chosen().as_deref(), Some("devstral:24b"));
        c.move_by(1);
        assert_eq!(c.chosen().as_deref(), Some("qwen3-coder:30b"));
    }

    /// A filter that leaves nothing chooses nothing, rather than the last thing it left.
    #[test]
    fn a_filter_that_leaves_nothing_chooses_nothing() {
        let mut c = models();
        for ch in "zzz".chars() {
            c.push(ch);
        }
        assert!(c.shown().is_empty());
        assert_eq!(c.chosen(), None);
    }

    /// The rows shown follow the highlight.
    /// A window pinned to the top would show the same names however far down the highlight goes.
    #[test]
    fn the_rows_shown_follow_the_highlight() {
        let many: Vec<Choice> = (0..40)
            .map(|i| Choice::new(format!("m{i:02}"), ""))
            .collect();
        let mut c = Chooser::new("model", "model", many, None);
        assert_eq!(c.window(), (0, ROWS));
        c.move_by(30);
        let (first, rows) = c.window();
        assert!(first <= 30 && 30 < first + rows, "{first} {rows}");
        // At the end it stops rather than scrolling past the last name.
        c.move_by(9);
        let (first, rows) = c.window();
        assert_eq!(first + rows, 40);
    }
}
