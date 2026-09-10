//! What has been learned about a project, kept between sessions.
//!
//! An agent rediscovers the same things every session: where the code lives, what the
//! test command is, which module owns what. Each rediscovery is a handful of searches and
//! file reads, and those are what fill the window - so the session that has learned the
//! most about a project is also the one with the least room left to use it.
//!
//! What is written here is the opposite: a small number of short statements, each with
//! where it came from, that can be put in front of a model for the price of a paragraph.
//! It is a cache of conclusions, not of files.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// What kind of thing was learned. The kinds are few on purpose: a taxonomy nobody can
/// keep in their head produces items nobody can find.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// Where things are and what owns what.
    Layout,
    /// How the project is built, tested, checked.
    Build,
    /// How code here is written: naming, error handling, the shape of a test.
    Convention,
    /// Something that went wrong here before and would go wrong again.
    Pitfall,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Layout => "layout",
            Kind::Build => "build",
            Kind::Convention => "convention",
            Kind::Pitfall => "pitfall",
        }
    }
}

/// One thing learned.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub kind: Kind,
    /// One sentence. Anything longer is a document, and a document belongs in the
    /// repository where a person can edit it.
    pub text: String,
    /// Where it came from: a path, a command, a session id. What makes an item
    /// checkable rather than folklore.
    #[serde(default)]
    pub source: String,
    /// Seconds since the epoch, so an item can be aged out or shown by recency.
    #[serde(default)]
    pub learned_at: u64,
}

/// Everything known about one project.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Digest {
    /// Keyed by a short slug so the same fact learned twice replaces itself rather than
    /// accumulating. The slug is derived from the text, which is crude and enough.
    #[serde(default)]
    pub items: BTreeMap<String, Item>,
    /// Questions still worth answering, oldest first. The work is persistent: a session
    /// that ends mid-discovery leaves the rest for the next one.
    #[serde(default)]
    pub questions: Vec<String>,
    /// Things worth telling the user, not yet told.
    #[serde(default)]
    pub suggestions: Vec<String>,
    #[serde(skip)]
    path: Option<PathBuf>,
}

/// How many items are kept. Past this the oldest go: the point is a paragraph a model can
/// read, not an archive.
const MAX_ITEMS: usize = 40;

/// How many characters of digest may reach a preamble.
///
/// It sits in the system message beside the standing instructions, so it is paid on every
/// request of the session. Half of what the instructions may take: what a person wrote
/// deliberately outranks what a background task inferred.
pub const MAX_PREAMBLE_CHARS: usize = 1200;

impl Digest {
    pub fn open(state_dir: Option<PathBuf>, root: &Path) -> Self {
        let Some(path) = state_dir.map(|d| d.join("digest").join(format!("{}.json", key(root))))
        else {
            return Self::default();
        };
        let mut digest: Self = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        digest.path = Some(path);
        digest
    }

    /// Record something learned, replacing an earlier statement of the same thing.
    pub fn learn(&mut self, kind: Kind, text: &str, source: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let item = Item {
            kind,
            text: text.to_string(),
            source: source.trim().to_string(),
            learned_at: now_secs(),
        };
        // Keyed by kind and subject, so a later wording of the same fact replaces the
        // earlier one and a statement about the layout never displaces one about the
        // build. Two facts that share six words and a kind are the same fact stated
        // twice often enough that merging them is the better error.
        self.items
            .insert(format!("{}:{}", kind.label(), slug(text)), item);
        while self.items.len() > MAX_ITEMS {
            // The oldest goes. A project that has changed enough to make forty statements
            // stale has changed enough that the oldest of them is the wrong one.
            if let Some(oldest) = self
                .items
                .iter()
                .min_by_key(|(_, i)| i.learned_at)
                .map(|(k, _)| k.clone())
            {
                self.items.remove(&oldest);
            }
        }
    }

    /// Add a question to answer later, if it is not already waiting.
    pub fn ask(&mut self, question: &str) {
        let question = question.trim().to_string();
        if question.is_empty() || self.questions.iter().any(|q| q == &question) {
            return;
        }
        // Bounded, and the oldest goes: a queue that grows without limit is one nobody
        // ever reaches the end of.
        self.questions.push(question);
        while self.questions.len() > 20 {
            self.questions.remove(0);
        }
    }

    pub fn next_question(&mut self) -> Option<String> {
        if self.questions.is_empty() {
            None
        } else {
            Some(self.questions.remove(0))
        }
    }

    pub fn suggest(&mut self, text: &str) {
        let text = text.trim().to_string();
        if text.is_empty() || self.suggestions.iter().any(|s| s == &text) {
            return;
        }
        self.suggestions.push(text);
        while self.suggestions.len() > 10 {
            self.suggestions.remove(0);
        }
    }

    /// Take the suggestions, leaving none: they are told once.
    pub fn take_suggestions(&mut self) -> Vec<String> {
        std::mem::take(&mut self.suggestions)
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// What goes in front of the model: the statements, by kind, bounded.
    ///
    /// Bounded by cutting whole items rather than characters: half a statement about a
    /// project is worse than one fewer statement.
    pub fn preamble(&self, max_chars: usize) -> Option<String> {
        if self.items.is_empty() {
            return None;
        }
        let mut by_kind: BTreeMap<Kind, Vec<&Item>> = BTreeMap::new();
        for item in self.items.values() {
            by_kind.entry(item.kind).or_default().push(item);
        }
        let mut out = String::new();
        for (kind, mut items) in by_kind {
            // Most recently learned first: what was found today is likelier to be true
            // than what was found before the last three commits.
            items.sort_by_key(|i| std::cmp::Reverse(i.learned_at));
            for item in items {
                let line = format!("- {}: {}\n", kind.label(), item.text);
                if out.len() + line.len() > max_chars {
                    break;
                }
                out.push_str(&line);
            }
        }
        (!out.is_empty()).then(|| out.trim_end().to_string())
    }

    /// The whole thing, with its provenance, for a person asking.
    pub fn describe(&self) -> String {
        if self.items.is_empty() && self.questions.is_empty() {
            return "nothing has been learned about this project yet".to_string();
        }
        let mut lines: Vec<String> = self
            .items
            .values()
            .map(|i| {
                let from = if i.source.is_empty() {
                    String::new()
                } else {
                    format!("  [{}]", i.source)
                };
                format!("{}: {}{from}", i.kind.label(), i.text)
            })
            .collect();
        lines.sort();
        if !self.questions.is_empty() {
            lines.push(format!("still to look into: {}", self.questions.join("; ")));
        }
        lines.join("\n")
    }

    pub fn save(&self) {
        let Some(path) = &self.path else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(text) = serde_json::to_string(self) {
            let _ = std::fs::write(path, text);
        }
    }

    /// Forget everything about this project.
    pub fn forget(&mut self) {
        self.items.clear();
        self.questions.clear();
        self.suggestions.clear();
        self.save();
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A key for a statement, so learning the same thing twice replaces it.
fn slug(text: &str) -> String {
    text.to_lowercase()
        .split_whitespace()
        .take(6)
        .collect::<Vec<_>>()
        .join("-")
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-')
        .collect()
}

/// A file name for a project root that says nothing about the path, as elsewhere.
fn key(root: &Path) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in root.as_os_str().as_encoded_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fact stated again, in whatever words, replaces the earlier statement rather
    /// than sitting beside it: a digest with three wordings of the test command is one
    /// nobody trusts.
    #[test]
    fn restating_a_fact_replaces_it_and_a_different_subject_does_not() {
        let mut d = Digest::default();
        d.learn(Kind::Build, "The test command is cargo test", "Cargo.toml");
        d.learn(Kind::Build, "The test command is cargo test --all", "CI");
        assert_eq!(d.items.len(), 1);
        assert!(d.describe().contains("--all"));
        assert!(d.describe().contains("[CI]"));

        // A different subject of the same kind is a different item.
        d.learn(Kind::Build, "Formatting is checked by cargo fmt", "CI");
        assert_eq!(d.items.len(), 2);
        // And so is the same subject under another kind.
        d.learn(Kind::Layout, "The test command is where tests live", "");
        assert_eq!(d.items.len(), 3);
    }

    /// The digest is a paragraph, not an archive: past its size the oldest goes.
    #[test]
    fn it_stays_small_enough_to_read() {
        let mut d = Digest::default();
        for i in 0..MAX_ITEMS + 10 {
            d.learn(
                Kind::Layout,
                &format!("statement number {i} about the code"),
                "",
            );
        }
        assert_eq!(d.items.len(), MAX_ITEMS);
        let preamble = d.preamble(MAX_PREAMBLE_CHARS).unwrap();
        assert!(preamble.len() <= MAX_PREAMBLE_CHARS);
        // Whole statements only: a half-sentence about a project is worse than one less.
        assert!(preamble.lines().all(|l| l.starts_with("- ")));
    }

    /// The work outlives the session: a question left unanswered is still there next time.
    #[test]
    fn questions_are_a_queue_that_survives() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        let mut d = Digest::open(Some(dir.path().to_path_buf()), &root);
        d.ask("what runs the tests here");
        d.ask("what runs the tests here");
        d.ask("where do the HTTP routes live");
        assert_eq!(d.questions.len(), 2);
        assert_eq!(
            d.next_question().as_deref(),
            Some("what runs the tests here")
        );
        d.save();

        let back = Digest::open(Some(dir.path().to_path_buf()), &root);
        assert_eq!(back.questions, vec!["where do the HTTP routes live"]);
    }

    #[test]
    fn suggestions_are_told_once() {
        let mut d = Digest::default();
        d.suggest("add the test command to .verve.md");
        d.suggest("add the test command to .verve.md");
        assert_eq!(d.take_suggestions().len(), 1);
        assert!(d.take_suggestions().is_empty());
    }

    #[test]
    fn nothing_learned_puts_nothing_in_front_of_the_model() {
        assert!(Digest::default().preamble(MAX_PREAMBLE_CHARS).is_none());
    }
}
