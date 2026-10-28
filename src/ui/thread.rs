//! The conversation as entries, and the rows they are drawn as.
//!
//! Every entry carries a revision that changes whenever the entry does, so what it was
//! drawn as can be kept and drawn again until it changes. A frame then costs the entries
//! that moved, not the whole session.

use std::collections::HashMap;
use std::ops::{Deref, Index, IndexMut};
use std::sync::Arc;

use ratatui::text::Line;

use crate::ui::chat::Entry;

/// The entries of the conversation, oldest first, each with its revision.
#[derive(Debug, Default)]
pub struct Thread {
    entries: Vec<Entry>,
    revs: Vec<u64>,
    last: u64,
}

impl Thread {
    fn fresh(&mut self) -> u64 {
        self.last += 1;
        self.last
    }

    pub fn push(&mut self, entry: Entry) {
        let rev = self.fresh();
        self.entries.push(entry);
        self.revs.push(rev);
    }

    pub fn extend(&mut self, entries: impl IntoIterator<Item = Entry>) {
        for entry in entries {
            self.push(entry);
        }
    }

    pub fn remove(&mut self, at: usize) -> Entry {
        self.revs.remove(at);
        self.entries.remove(at)
    }

    pub fn truncate(&mut self, len: usize) {
        self.entries.truncate(len);
        self.revs.truncate(len);
    }

    pub fn clear(&mut self) {
        self.truncate(0);
    }

    /// The newest entry, to be changed: its revision moves.
    pub fn last_mut(&mut self) -> Option<&mut Entry> {
        let at = self.entries.len().checked_sub(1)?;
        Some(&mut self[at])
    }

    /// The revision of the entry at `at`.
    pub fn rev(&self, at: usize) -> u64 {
        self.revs[at]
    }
}

impl Deref for Thread {
    type Target = [Entry];

    fn deref(&self) -> &[Entry] {
        &self.entries
    }
}

impl Index<usize> for Thread {
    type Output = Entry;

    fn index(&self, at: usize) -> &Entry {
        &self.entries[at]
    }
}

/// Reaching an entry to change it moves its revision.
impl IndexMut<usize> for Thread {
    fn index_mut(&mut self, at: usize) -> &mut Entry {
        let rev = self.fresh();
        self.revs[at] = rev;
        &mut self.entries[at]
    }
}

/// How one entry was drawn, and for what.
#[derive(Debug)]
struct Drawn {
    width: usize,
    open: bool,
    here: bool,
    rows: Arc<Vec<Line<'static>>>,
}

/// What each revision was drawn as, kept until the entry changes or leaves.
#[derive(Debug, Default)]
pub struct Drawings {
    by_rev: HashMap<u64, Drawn>,
    /// How many entries have been drawn, for tests that a frame drew only what changed.
    #[cfg(test)]
    pub drawn: usize,
}

impl Drawings {
    /// The rows of an entry wrapped to `width`, drawn again only when the entry, the width,
    /// its unfolding or the pointing mark changed.
    pub fn rows(
        &mut self,
        rev: u64,
        entry: &Entry,
        width: usize,
        open: bool,
        here: bool,
    ) -> Arc<Vec<Line<'static>>> {
        if let Some(d) = self.by_rev.get(&rev) {
            if d.width == width && d.open == open && d.here == here {
                return d.rows.clone();
            }
        }
        #[cfg(test)]
        {
            self.drawn += 1;
        }
        let rows = Arc::new(crate::ui::chat::wrap(entry.render(open, here), width));
        self.by_rev.insert(
            rev,
            Drawn {
                width,
                open,
                here,
                rows: rows.clone(),
            },
        );
        rows
    }

    /// Drop what was drawn for revisions the thread no longer holds.
    ///
    /// Only once there are more drawings than entries, so a frame that changed nothing does
    /// not walk the thread for it.
    pub fn forget_gone(&mut self, thread: &Thread) {
        if self.by_rev.len() <= thread.len() * 2 {
            return;
        }
        let live: std::collections::HashSet<u64> = thread.revs.iter().copied().collect();
        self.by_rev.retain(|rev, _| live.contains(rev));
    }
}

/// The pane's rows, as the blocks each entry was drawn as.
///
/// Held as shared blocks rather than one list, so laying out a frame copies a pointer per
/// entry instead of every row of the session.
#[derive(Debug, Default)]
pub struct Pane {
    blocks: Vec<Arc<Vec<Line<'static>>>>,
    starts: Vec<usize>,
    len: usize,
}

impl Pane {
    pub fn push(&mut self, rows: Arc<Vec<Line<'static>>>) {
        self.starts.push(self.len);
        self.len += rows.len();
        self.blocks.push(rows);
    }

    /// How many rows there are.
    pub fn len(&self) -> usize {
        self.len
    }

    /// The row at `at`.
    pub fn get(&self, at: usize) -> Option<&Line<'static>> {
        if at >= self.len {
            return None;
        }
        let block = self.starts.partition_point(|&s| s <= at) - 1;
        self.blocks[block].get(at - self.starts[block])
    }

    /// The row at which block `at` begins.
    pub fn start_of(&self, at: usize) -> Option<usize> {
        self.starts.get(at).copied()
    }

    /// The rows from `from` up to but not including `to`.
    pub fn rows(&self, from: usize, to: usize) -> impl Iterator<Item = &Line<'static>> {
        (from..to.min(self.len)).filter_map(|at| self.get(at))
    }
}

/// One block per line, for tests that lay out rows by hand.
#[cfg(test)]
impl From<Vec<Line<'static>>> for Pane {
    fn from(lines: Vec<Line<'static>>) -> Self {
        let mut pane = Pane::default();
        for line in lines {
            pane.push(Arc::new(vec![line]));
        }
        pane
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Changing an entry, by any route, gives it a revision it has never had.
    #[test]
    fn a_changed_entry_has_a_new_revision() {
        let mut t = Thread::default();
        t.push(Entry::Notice("a".into()));
        t.push(Entry::Reasoning("b".into()));
        let (a, b) = (t.rev(0), t.rev(1));
        assert_ne!(a, b);
        if let Some(Entry::Reasoning(s)) = t.last_mut() {
            s.push('c');
        }
        assert_ne!(t.rev(1), b);
        t[0] = Entry::Notice("z".into());
        assert_ne!(t.rev(0), a);
        // Removing one keeps the revisions of the others with their entries.
        let kept = t.rev(1);
        t.remove(0);
        assert_eq!(t.rev(0), kept);
    }

    /// An entry is drawn once until it, the width, or the way it is shown changes.
    #[test]
    fn an_unchanged_entry_is_not_drawn_again() {
        let mut t = Thread::default();
        t.push(Entry::Notice("hello".into()));
        let mut d = Drawings::default();
        let first = d.rows(t.rev(0), &t[0], 40, false, false);
        assert!(Arc::ptr_eq(
            &first,
            &d.rows(t.rev(0), &t[0], 40, false, false)
        ));
        assert!(!Arc::ptr_eq(
            &first,
            &d.rows(t.rev(0), &t[0], 30, false, false)
        ));
        assert!(!Arc::ptr_eq(
            &first,
            &d.rows(t.rev(0), &t[0], 30, false, true)
        ));
        t[0] = Entry::Notice("changed".into());
        let after = d.rows(t.rev(0), &t[0], 30, false, true);
        assert_eq!(crate::ui::chat::line_text(&after[0]), "▌· changed");
    }

    /// Drawings of entries that left go once they outnumber the entries.
    #[test]
    fn drawings_of_gone_entries_are_dropped() {
        let mut t = Thread::default();
        let mut d = Drawings::default();
        for i in 0..10 {
            t.push(Entry::Notice(format!("{i}")));
            d.rows(t.rev(i), &t[i], 40, false, false);
        }
        t.truncate(2);
        d.forget_gone(&t);
        assert_eq!(d.by_rev.len(), 2);
    }

    /// A row is found in whichever block holds it.
    #[test]
    fn a_row_is_found_across_blocks() {
        let mut p = Pane::default();
        p.push(Arc::new(vec![Line::from("a0"), Line::from("a1")]));
        p.push(Arc::new(vec![]));
        p.push(Arc::new(vec![Line::from("c0")]));
        assert_eq!(p.len(), 3);
        let text = |at| p.get(at).map(crate::ui::chat::line_text);
        assert_eq!(text(0).as_deref(), Some("a0"));
        assert_eq!(text(1).as_deref(), Some("a1"));
        assert_eq!(text(2).as_deref(), Some("c0"));
        assert_eq!(text(3), None);
        assert_eq!(p.start_of(2), Some(2));
        assert_eq!(p.rows(1, 10).count(), 2);
    }
}
