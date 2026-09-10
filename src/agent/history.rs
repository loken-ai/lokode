//! What each model has actually managed, in this project.
//!
//! The escalation ladder reacts: it raises the model after a change has already failed to
//! compile, which costs one bad step every time. What the steps recorded before it can add
//! is the memory of those steps - if the cheap model has failed most of its writes in this
//! repository, the next turn can start where the last one ended up rather than paying the
//! same bad step again.
//!
//! Deliberately the smallest thing that could work: counts of writes and of writes that
//! checked out, per model, per project, kept on this machine. No training, no features
//! beyond the outcome, nothing that leaves the disk. A model that cannot be explained in
//! one line has no place deciding which model answers.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// How many recent writes are remembered per model.
///
/// A project changes - its code, its conventions, the models available - so evidence has
/// to age out. Fifty is enough to be more than noise and short enough that a month-old
/// judgement does not decide today's.
const REMEMBERED: usize = 50;

/// How much evidence before the memory is allowed to decide anything.
const ENOUGH: usize = 4;

/// Above this share of failed writes, the cheap model is not worth the bad step.
const TOO_MANY_FAILURES: f64 = 0.5;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Outcomes {
    /// Most recent last: true for a write the language server was happy with.
    #[serde(default)]
    pub recent: VecDeque<bool>,
}

impl Outcomes {
    fn record(&mut self, passed: bool) {
        self.recent.push_back(passed);
        while self.recent.len() > REMEMBERED {
            self.recent.pop_front();
        }
    }

    /// The share of remembered writes that did not check out, once there are enough of
    /// them to mean anything.
    pub fn failure_rate(&self) -> Option<f64> {
        if self.recent.len() < ENOUGH {
            return None;
        }
        let failed = self.recent.iter().filter(|ok| !**ok).count();
        Some(failed as f64 / self.recent.len() as f64)
    }
}

/// What every model has managed here.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelHistory {
    #[serde(default)]
    pub by_model: BTreeMap<String, Outcomes>,
    #[serde(skip)]
    path: Option<PathBuf>,
}

impl ModelHistory {
    /// Read what is remembered about `root`, or start empty.
    ///
    /// Kept beside the state rather than in the repository: it is a record of what
    /// happened on this machine, not a fact about the project that others should inherit.
    pub fn open(state_dir: Option<PathBuf>, root: &Path) -> Self {
        let Some(path) = state_dir.map(|d| d.join("models").join(format!("{}.json", key(root))))
        else {
            return Self::default();
        };
        let mut history: Self = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        history.path = Some(path);
        history
    }

    pub fn record(&mut self, model: &str, passed: bool) {
        self.by_model
            .entry(model.to_string())
            .or_default()
            .record(passed);
    }

    /// Whether starting on `model` is worth the risk of a step that has to be redone.
    ///
    /// `None` when there is not enough evidence, which is the usual answer and the one
    /// that leaves the configuration's choice alone.
    pub fn should_avoid(&self, model: &str) -> Option<bool> {
        let rate = self.by_model.get(model)?.failure_rate()?;
        Some(rate > TOO_MANY_FAILURES)
    }

    /// One line per model, for a person asking why.
    pub fn describe(&self) -> String {
        if self.by_model.is_empty() {
            return "nothing has been written here yet".to_string();
        }
        self.by_model
            .iter()
            .map(|(model, o)| {
                let total = o.recent.len();
                let ok = o.recent.iter().filter(|p| **p).count();
                match o.failure_rate() {
                    None => {
                        format!("{model}: {ok} of {total} writes checked out, too few to judge")
                    }
                    Some(_) => format!("{model}: {ok} of {total} writes checked out"),
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Write it back, quietly: losing this costs a memory, not a session.
    pub fn save(&self) {
        let Some(path) = &self.path else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(text) = serde_json::to_string(self) {
            let _ = std::fs::write(path, text);
        }
    }
}

/// A file name for a project root that is stable and says nothing about the path.
///
/// The directory listing of a state folder should not be a list of what someone is
/// working on.
pub(super) fn key(root: &Path) -> String {
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

    #[test]
    fn too_little_evidence_decides_nothing() {
        let mut h = ModelHistory::default();
        h.record("small", false);
        h.record("small", false);
        assert_eq!(h.should_avoid("small"), None);
        assert_eq!(h.should_avoid("never-seen"), None);
        h.record("small", false);
        h.record("small", false);
        assert_eq!(h.should_avoid("small"), Some(true));
    }

    #[test]
    fn a_model_that_mostly_works_is_not_avoided() {
        let mut h = ModelHistory::default();
        for passed in [true, true, true, false, true] {
            h.record("small", passed);
        }
        assert_eq!(h.should_avoid("small"), Some(false));
    }

    /// A project changes, so evidence ages out rather than accumulating for ever.
    #[test]
    fn only_the_recent_writes_are_remembered() {
        let mut h = ModelHistory::default();
        for _ in 0..REMEMBERED + 10 {
            h.record("small", false);
        }
        for _ in 0..REMEMBERED {
            h.record("small", true);
        }
        assert_eq!(h.by_model["small"].recent.len(), REMEMBERED);
        assert_eq!(h.should_avoid("small"), Some(false));
    }

    #[test]
    fn what_is_remembered_survives_a_round_trip_and_names_no_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("some/private/project");
        let mut h = ModelHistory::open(Some(dir.path().to_path_buf()), &root);
        for passed in [true, false, true, true] {
            h.record("small", passed);
        }
        h.save();

        let listing: Vec<String> = std::fs::read_dir(dir.path().join("models"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(listing.len(), 1);
        assert!(!listing[0].contains("project"), "{listing:?}");

        let back = ModelHistory::open(Some(dir.path().to_path_buf()), &root);
        assert_eq!(back.by_model["small"].recent.len(), 4);
        assert!(back.describe().contains("3 of 4 writes checked out"));
    }
}
