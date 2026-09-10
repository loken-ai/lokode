//! What fixed this before.
//!
//! The same project fails the same way. A missing trait import, a field renamed in one
//! place and read in another, a test fixture that has to be updated beside the code: the
//! compiler says the same thing it said last week, and the model works it out again from
//! nothing, at the cost of the steps that took last time.
//!
//! So the pair is written down. When a change does not check out, the complaint is kept;
//! when a later change makes the same complaint go away, what that change did is filed
//! against it. The next time the complaint comes back, it arrives with the note.
//!
//! Small and local by construction. A signature and a sentence, on this machine, beside
//! the rest of the state, never in the repository: what fixed it here says nothing about
//! anyone else's project and has no business travelling.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// How many pairs are kept. A project's failures repeat; its whole history does not need
/// to.
const REMEMBERED: usize = 40;

/// How much of a complaint makes its signature.
const SIGNATURE_CHARS: usize = 160;

/// One complaint and what made it go away.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Repair {
    pub signature: String,
    /// What the change that fixed it did, in the words of the tool that did it.
    pub fix: String,
    pub learned_at: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Repairs {
    #[serde(default)]
    pub items: VecDeque<Repair>,
    /// The complaint waiting for a fix, if a change has just failed to check out.
    #[serde(skip)]
    open: Option<String>,
    #[serde(skip)]
    path: Option<PathBuf>,
}

impl Repairs {
    pub fn open(state_dir: Option<PathBuf>, root: &Path) -> Self {
        let Some(path) = state_dir.map(|d| {
            d.join("repairs")
                .join(format!("{}.json", super::history::key(root)))
        }) else {
            return Self::default();
        };
        let mut me: Self = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        me.path = Some(path);
        me
    }

    /// A change did not check out. Hold what the check said, waiting for what fixes it.
    pub fn failed(&mut self, said: &str) {
        self.open = Some(signature(said));
    }

    /// A change checked out. If something was waiting, this is what fixed it.
    ///
    /// Only the first clean step after a failure counts. What comes after it is the work
    /// carrying on, and filing that against the complaint would teach the wrong lesson.
    pub fn fixed(&mut self, what: &str) {
        let Some(signature) = self.open.take() else {
            return;
        };
        let fix = what.lines().next().unwrap_or(what).trim().to_string();
        if fix.is_empty() {
            return;
        }
        self.items.retain(|r| r.signature != signature);
        self.items.push_back(Repair {
            signature,
            fix,
            learned_at: now(),
        });
        while self.items.len() > REMEMBERED {
            self.items.pop_front();
        }
    }

    /// What fixed this complaint last time, when this project has seen it before.
    pub fn recall(&self, said: &str) -> Option<&str> {
        let signature = signature(said);
        self.items
            .iter()
            .rev()
            .find(|r| r.signature == signature)
            .map(|r| r.fix.as_str())
    }

    /// One line per pair, for a person asking what has been learned.
    pub fn describe(&self) -> String {
        if self.items.is_empty() {
            return "nothing has been fixed twice here yet".to_string();
        }
        self.items
            .iter()
            .rev()
            .map(|r| format!("  {} -> {}", r.signature, r.fix))
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn save(&self) {
        let Some(path) = self.path.as_ref() else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, text);
        }
    }
}

/// What makes two complaints the same complaint.
///
/// Line and column numbers, and the paths in front of them, are where it happened rather
/// than what happened: the same mistake made twice is the same mistake at two places. What
/// is left is the wording and the names in it, which is what a person would recognise.
fn signature(said: &str) -> String {
    let line = said
        .lines()
        .find(|l| {
            let l = l.trim();
            !l.is_empty() && !l.starts_with("the language server reports:") && !l.starts_with("-->")
        })
        .unwrap_or("")
        .trim();
    // A leading "path:line:column:" says where, and the rest says what.
    let line = strip_place(line);
    let mut out = String::with_capacity(line.len());
    let mut in_number = false;
    for c in line.chars() {
        if c.is_ascii_digit() {
            in_number = true;
            continue;
        }
        if in_number {
            in_number = false;
            // A run of digits leaves one mark, so "line 12" and "line 480" agree.
            out.push('#');
        }
        out.push(c);
    }
    if in_number {
        out.push('#');
    }
    let out = out.split_whitespace().collect::<Vec<_>>().join(" ");
    out.chars().take(SIGNATURE_CHARS).collect()
}

/// The complaint with any "path:line:column:" in front of it removed.
fn strip_place(line: &str) -> &str {
    let mut rest = line;
    // Two numbered fields at most: a line and a column.
    for _ in 0..2 {
        let Some((head, tail)) = rest.rsplit_once(':') else {
            break;
        };
        // Only a numeric field counts: "error: cannot find" must keep its wording.
        let last = head.rsplit(':').next().unwrap_or("");
        if last.is_empty() || !last.chars().all(|c| c.is_ascii_digit()) {
            break;
        }
        let _ = tail;
        rest = head;
    }
    if std::ptr::eq(rest, line) {
        return line;
    }
    // `rest` is now the part before the numbers; what is wanted is what follows them.
    let after = &line[rest.len()..];
    let after = after.trim_start_matches(|c: char| c.is_ascii_digit() || c == ':');
    after.trim()
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same mistake at two places is the same mistake: where it happened is not what
    /// happened.
    #[test]
    fn a_complaint_is_recognised_wherever_it_happened() {
        let a = "src/a.rs:12:5: cannot find value `foo` in this scope";
        let b = "src/b.rs:480:9: cannot find value `foo` in this scope";
        assert_eq!(signature(a), signature(b));
        let other = "src/a.rs:12:5: cannot find value `bar` in this scope";
        assert_ne!(signature(a), signature(other));
    }

    #[test]
    fn what_fixed_it_comes_back_when_it_happens_again() {
        let mut r = Repairs::default();
        r.failed("src/a.rs:12:5: cannot find value `foo` in this scope");
        r.fixed("edited src/a.rs\nadded the missing use");
        assert_eq!(
            r.recall("src/b.rs:9:1: cannot find value `foo` in this scope"),
            Some("edited src/a.rs")
        );
        assert_eq!(r.recall("something else entirely"), None);
    }

    /// Only the first clean step after a failure is the fix. What comes after it is the
    /// work carrying on.
    #[test]
    fn only_the_step_that_fixed_it_is_filed_against_it() {
        let mut r = Repairs::default();
        r.failed("cannot find value `foo` in this scope");
        r.fixed("edited src/a.rs");
        r.fixed("edited src/b.rs");
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.items[0].fix, "edited src/a.rs");
    }

    #[test]
    fn a_clean_step_with_nothing_waiting_teaches_nothing() {
        let mut r = Repairs::default();
        r.fixed("edited src/a.rs");
        assert!(r.items.is_empty());
    }
}
