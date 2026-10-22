//! A history of what the agent changed, kept beside the project rather than in it.
//!
//! Approving a diff means agreeing to one change. It does not mean you can still see, four
//! turns later, everything that has happened since - and it certainly does not mean you can
//! put any of it back. So every change the agent makes is committed to a git repository whose
//! directory lives under the session's own state, with the project as its work tree.
//!
//! Nothing of the user's is touched: their `.git` is a different directory and is excluded
//! from this one, their index is untouched, and their history gains nothing. What they get is
//! `/diff`, which is everything the agent has done this session, and `/undo`, which puts a
//! turn back.
//!
//! The one operation here that writes to the user's files is `undo`. It commits the current
//! state first, and then moves forward - a new commit carrying the old tree, rather than a
//! rewind - so no state ever becomes unreachable. That matters because a file may have been
//! edited by hand between the agent's change and the decision to take it back, and a history
//! that discards it to make the undo tidy has made the wrong trade.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::process::Command;

/// Written into the shadow repository so it never descends into the project's own.
const EXCLUDES: &str = "/.git/\n";

pub struct Checkpoints {
    git_dir: PathBuf,
    root: PathBuf,
    /// The commit made when the session opened; `diff` is measured from here.
    baseline: String,
}

/// What a snapshot came to.
#[derive(Debug, PartialEq, Eq)]
pub enum Snapshot {
    /// A commit was made, and this is it.
    Recorded(String),
    /// The project is identical to the last snapshot, so there was nothing to record.
    Unchanged,
}

/// A turn that changed something and has not been put back.
#[derive(Debug)]
pub struct Turn {
    pub turn: u32,
    /// What that turn itself changed.
    pub files: Vec<String>,
}

#[derive(Debug)]
pub struct Undone {
    /// What was put back.
    pub turn: u32,
    pub files: Vec<String>,
}

impl Checkpoints {
    /// Open the history for a session, taking a baseline of the project as it is now.
    ///
    /// Fails rather than degrading if git is missing: a feature that silently does not record
    /// anything is worse than one that says it is not available, because the moment you find
    /// out is the moment you needed it.
    pub async fn open(session_id: &str, root: &Path) -> Result<Self> {
        let git_dir = crate::paths::data_dir()
            .ok_or_else(|| anyhow::anyhow!("no home directory to keep checkpoints in"))?
            .join("checkpoints")
            .join(session_id);
        Self::open_at(git_dir, root).await
    }

    /// Open a history in a named directory.
    ///
    /// Where it lives is a parameter rather than something read from the environment, so
    /// that two of these can exist at once without either having to know about the other.
    pub async fn open_at(git_dir: PathBuf, root: &Path) -> Result<Self> {
        std::fs::create_dir_all(&git_dir)
            .with_context(|| format!("creating {}", git_dir.display()))?;

        let me = Self {
            git_dir,
            root: root.to_path_buf(),
            baseline: String::new(),
        };

        if !me.git_dir.join("HEAD").exists() {
            me.git(&["init", "--quiet"]).await.context("git init")?;
            std::fs::write(me.git_dir.join("info").join("exclude"), EXCLUDES).ok();
        }

        let baseline = match me.commit("session baseline", 0).await? {
            Snapshot::Recorded(sha) => sha,
            // Reopening an existing history with nothing changed since.
            Snapshot::Unchanged => me.head().await?,
        };
        Ok(Self { baseline, ..me })
    }

    /// Record the project as it stands.
    pub async fn snapshot(&self, label: &str, turn: u32) -> Result<Snapshot> {
        self.commit(label, turn).await
    }

    /// Everything that has changed since the session opened.
    pub async fn diff(&self) -> Result<String> {
        self.commit("before diff", 0).await?;
        let head = self.head().await?;
        if head == self.baseline {
            return Ok(String::new());
        }
        self.git(&["diff", &self.baseline, &head]).await
    }

    /// Put back the most recent turn that has not been put back already.
    ///
    /// The current state is committed first, and the undo is itself a new commit carrying
    /// the old tree rather than a rewind. So nothing ever becomes unreachable: a file edited
    /// by hand between the agent's change and the decision to take it back is still a commit
    /// in this history, and calling this again walks one turn further back rather than
    /// undoing the same one twice.
    pub async fn undo(&self) -> Result<Option<Undone>> {
        let Some(turn) = self.turns().await?.first().map(|t| t.turn) else {
            return Ok(None);
        };
        self.undo_turn(turn).await
    }

    /// The turns that changed something and have not been put back, newest first.
    ///
    /// What a person needs to choose one: which turn, and what it touched. Undoing is
    /// otherwise a single step into the dark, offered without saying where it lands.
    pub async fn turns(&self) -> Result<Vec<Turn>> {
        self.commit("before listing", 0).await?;
        let entries = self.entries().await?;
        let undone: Vec<u32> = entries.iter().filter_map(|(_, s)| undone_turn(s)).collect();
        let mut out: Vec<Turn> = Vec::new();
        for turn in entries.iter().map(|(_, s)| turn_of(s)) {
            if turn == 0 || undone.contains(&turn) || out.iter().any(|t| t.turn == turn) {
                continue;
            }
            let Some(target) = Self::before(&entries, turn) else {
                continue;
            };
            // What that turn changed, not what everything since changed: the second is
            // what undoing it costs, and it is said at the point of choosing instead.
            let last = entries
                .iter()
                .find(|(_, s)| turn_of(s) == turn)
                .map(|(sha, _)| sha.clone())
                .unwrap_or_else(|| "HEAD".to_string());
            out.push(Turn {
                turn,
                files: self.changed(&target, &last).await?,
            });
        }
        Ok(out)
    }

    /// Put the project back to how it stood before `turn`.
    pub async fn undo_turn(&self, turn: u32) -> Result<Option<Undone>> {
        self.commit("before undo", 0).await?;
        let entries = self.entries().await?;
        let Some(target) = Self::before(&entries, turn) else {
            return Ok(None);
        };
        let files = self.changed(&target, "HEAD").await?;

        // The only place this writes to the user's files. `read-tree -u` moves the working
        // tree to the old state - removing files that turn created - without moving the
        // branch, so the commit below records the undo as one more step forward.
        self.git(&["read-tree", "-u", "--reset", &target]).await?;
        self.git(&[
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            &format!("lokode-turn:0 undo of turn {turn}"),
        ])
        .await?;

        Ok(Some(Undone { turn, files }))
    }

    /// Every commit since the baseline, newest first, as its hash and its subject.
    async fn entries(&self) -> Result<Vec<(String, String)>> {
        let log = self
            .git(&["log", "--format=%H %s", &format!("{}..HEAD", self.baseline)])
            .await?;
        Ok(log
            .lines()
            .filter_map(|line| line.split_once(' '))
            .map(|(sha, subject)| (sha.to_string(), subject.to_string()))
            .collect())
    }

    /// The state to return to for `turn`: the one before its first commit.
    fn before(entries: &[(String, String)], turn: u32) -> Option<String> {
        entries
            .iter()
            .rev()
            .find(|(_, s)| turn_of(s) == turn)
            .map(|(sha, _)| format!("{sha}^"))
    }

    async fn changed(&self, from: &str, to: &str) -> Result<Vec<String>> {
        Ok(self
            .git(&["diff", "--name-only", from, to])
            .await?
            .lines()
            .map(str::to_string)
            .collect())
    }

    async fn head(&self) -> Result<String> {
        Ok(self.git(&["rev-parse", "HEAD"]).await?.trim().to_string())
    }

    async fn commit(&self, label: &str, turn: u32) -> Result<Snapshot> {
        self.git(&["add", "--all", "."]).await.context("git add")?;
        // `diff --cached --quiet` exits 1 when the index differs from HEAD, which is how an
        // empty commit is avoided without parsing anything.
        let unchanged = self
            .git_status(&["diff", "--cached", "--quiet"])
            .await?
            .success();
        if unchanged && self.head().await.is_ok() {
            return Ok(Snapshot::Unchanged);
        }
        // The turn goes in the subject rather than a trailer, because the log is read back
        // one line per commit and a trailer is not on that line.
        self.git(&[
            "commit",
            "--quiet",
            "-m",
            &format!("lokode-turn:{turn} {label}"),
        ])
        .await
        .context("git commit")?;
        Ok(Snapshot::Recorded(self.head().await?))
    }

    fn base(&self) -> Command {
        let mut cmd = Command::new("git");
        cmd.env("GIT_DIR", &self.git_dir)
            .env("GIT_WORK_TREE", &self.root)
            // The user's identity is not this repository's business, and a machine without
            // one configured would otherwise fail to commit at all.
            .env("GIT_AUTHOR_NAME", "lokode")
            .env("GIT_AUTHOR_EMAIL", "lokode@localhost")
            .env("GIT_COMMITTER_NAME", "lokode")
            .env("GIT_COMMITTER_EMAIL", "lokode@localhost")
            .current_dir(&self.root)
            .kill_on_drop(true);
        cmd
    }

    async fn git(&self, args: &[&str]) -> Result<String> {
        let out = self.base().args(args).output().await?;
        if !out.status.success() {
            anyhow::bail!(
                "git {}: {}",
                args.first().copied().unwrap_or(""),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    async fn git_status(&self, args: &[&str]) -> Result<std::process::ExitStatus> {
        Ok(self.base().args(args).output().await?.status)
    }
}

/// The turn a commit records having taken back, if it is an undo.
fn undone_turn(subject: &str) -> Option<u32> {
    subject
        .split_once("undo of turn ")
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .and_then(|n| n.parse().ok())
}

/// The turn a commit belongs to, from the marker its subject starts with.
fn turn_of(subject: &str) -> u32 {
    subject
        .split_whitespace()
        .next()
        .and_then(|w| w.strip_prefix("lokode-turn:"))
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A history and a project, both private to one test.
    struct Fixture {
        store: TempDir,
        root: TempDir,
    }

    fn fixture() -> Fixture {
        Fixture {
            store: TempDir::new().unwrap(),
            root: TempDir::new().unwrap(),
        }
    }

    impl Fixture {
        async fn open(&self) -> Checkpoints {
            let root = self.root.path().canonicalize().unwrap();
            Checkpoints::open_at(self.store.path().join("history"), &root)
                .await
                .unwrap()
        }
    }

    fn write(root: &Path, name: &str, body: &str) {
        let p = root.join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, body).unwrap();
    }

    fn read(root: &Path, name: &str) -> String {
        std::fs::read_to_string(root.join(name)).unwrap()
    }

    #[tokio::test]
    async fn a_turn_can_be_taken_back() {
        let f = fixture();
        let root = f.root.path().canonicalize().unwrap();
        write(&root, "a.txt", "original\n");

        let cp = f.open().await;

        // Turn 1 changes a file and adds another.
        write(&root, "a.txt", "changed by the agent\n");
        write(&root, "b.txt", "new\n");
        assert!(matches!(
            cp.snapshot("write_file", 1).await.unwrap(),
            Snapshot::Recorded(_)
        ));

        let undone = cp.undo().await.unwrap().expect("something to undo");
        assert_eq!(undone.turn, 1);
        assert_eq!(read(&root, "a.txt"), "original\n");
        assert!(
            !root.join("b.txt").exists(),
            "a file the turn created should be gone again"
        );
    }

    /// Undoing is otherwise a step into the dark: which turn, and what it touched, is
    /// what a person needs to choose one.
    #[tokio::test]
    async fn the_turns_that_can_be_taken_back_are_named_with_what_they_touched() {
        let f = fixture();
        let root = f.root.path().canonicalize().unwrap();
        write(&root, "a.txt", "original\n");
        let cp = f.open().await;

        write(&root, "a.txt", "turn one\n");
        cp.snapshot("write_file", 1).await.unwrap();
        write(&root, "b.txt", "turn two\n");
        cp.snapshot("write_file", 2).await.unwrap();

        let turns = cp.turns().await.unwrap();
        assert_eq!(turns.iter().map(|t| t.turn).collect::<Vec<_>>(), vec![2, 1]);
        assert_eq!(turns[0].files, vec!["b.txt"]);
        assert_eq!(turns[1].files, vec!["a.txt"]);

        // An older turn chosen puts the project back to before it, which is what going
        // back to a point in a history means.
        let undone = cp.undo_turn(1).await.unwrap().expect("something to undo");
        assert_eq!(undone.turn, 1);
        assert_eq!(read(&root, "a.txt"), "original\n");
        assert!(!root.join("b.txt").exists(), "everything after it goes too");
        // And it is not offered again.
        assert!(cp.turns().await.unwrap().iter().all(|t| t.turn != 1));
    }

    /// Undoing must not be able to lose a change somebody made by hand in between. The
    /// guarantee is that nothing becomes unreachable, not that one keystroke reverses it:
    /// the state is still a commit, and the history only ever moves forward.
    #[tokio::test]
    async fn nothing_is_lost_when_a_turn_is_taken_back() {
        let f = fixture();
        let root = f.root.path().canonicalize().unwrap();
        write(&root, "a.txt", "original\n");
        let cp = f.open().await;

        write(&root, "a.txt", "agent\n");
        cp.snapshot("write_file", 1).await.unwrap();
        // Somebody edits the file themselves, after the agent and before the undo.
        write(&root, "a.txt", "and then a human\n");

        cp.undo().await.unwrap().expect("something to undo");
        assert_eq!(read(&root, "a.txt"), "original\n");

        // The hand edit is still in the history rather than orphaned by a rewind.
        let log = cp.git(&["log", "--all", "--format=%H"]).await.unwrap();
        let mut found = false;
        for sha in log.lines() {
            let show = cp.git(&["show", &format!("{sha}:a.txt")]).await;
            if show
                .map(|s| s.contains("and then a human"))
                .unwrap_or(false)
            {
                found = true;
            }
        }
        assert!(found, "the hand edit became unreachable");
    }

    /// Undoing twice walks two turns back rather than taking the same one twice.
    #[tokio::test]
    async fn undoing_again_goes_one_turn_further() {
        let f = fixture();
        let root = f.root.path().canonicalize().unwrap();
        write(&root, "a.txt", "v0\n");
        let cp = f.open().await;

        write(&root, "a.txt", "v1\n");
        cp.snapshot("edit_file", 1).await.unwrap();
        write(&root, "a.txt", "v2\n");
        cp.snapshot("edit_file", 2).await.unwrap();

        assert_eq!(cp.undo().await.unwrap().unwrap().turn, 2);
        assert_eq!(read(&root, "a.txt"), "v1\n");

        assert_eq!(cp.undo().await.unwrap().unwrap().turn, 1);
        assert_eq!(read(&root, "a.txt"), "v0\n");

        assert!(
            cp.undo().await.unwrap().is_none(),
            "there was a third turn to undo"
        );
    }

    #[tokio::test]
    async fn a_diff_shows_everything_since_the_session_opened() {
        let f = fixture();
        let root = f.root.path().canonicalize().unwrap();
        write(&root, "a.txt", "one\n");
        let cp = f.open().await;

        assert_eq!(
            cp.diff().await.unwrap(),
            "",
            "a fresh session has changed nothing"
        );

        write(&root, "a.txt", "two\n");
        cp.snapshot("edit_file", 1).await.unwrap();
        write(&root, "c.txt", "three\n");
        cp.snapshot("write_file", 2).await.unwrap();

        let diff = cp.diff().await.unwrap();
        assert!(diff.contains("a.txt"), "{diff}");
        assert!(diff.contains("c.txt"), "{diff}");
        assert!(diff.contains("-one"), "{diff}");
        assert!(diff.contains("+two"), "{diff}");
    }

    #[tokio::test]
    async fn nothing_to_undo_is_an_answer() {
        let f = fixture();
        let root = f.root.path().canonicalize().unwrap();
        write(&root, "a.txt", "one\n");
        let cp = f.open().await;
        assert!(cp.undo().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn an_unchanged_project_records_nothing() {
        let f = fixture();
        let root = f.root.path().canonicalize().unwrap();
        write(&root, "a.txt", "one\n");
        let cp = f.open().await;
        assert_eq!(
            cp.snapshot("bash", 1).await.unwrap(),
            Snapshot::Unchanged,
            "a tool that changed nothing left a commit behind"
        );
    }

    /// The project's own repository is none of this one's business.
    #[tokio::test]
    async fn the_projects_own_git_directory_is_left_alone() {
        let f = fixture();
        let root = f.root.path().canonicalize().unwrap();
        write(&root, ".git/HEAD", "ref: refs/heads/main\n");
        write(&root, "a.txt", "one\n");

        let cp = f.open().await;
        write(&root, "a.txt", "two\n");
        cp.snapshot("edit_file", 1).await.unwrap();

        let diff = cp.diff().await.unwrap();
        assert!(
            !diff.contains(".git/"),
            "the project's own repo was captured: {diff}"
        );
        assert_eq!(
            read(&root, ".git/HEAD"),
            "ref: refs/heads/main\n",
            "the project's own repo was disturbed"
        );
    }

    #[test]
    fn a_commit_declares_which_turn_it_belongs_to() {
        assert_eq!(turn_of("lokode-turn:7 write_file"), 7);
        assert_eq!(turn_of("lokode-turn:0 session baseline"), 0);
        assert_eq!(turn_of("a commit made by something else"), 0);
        assert_eq!(undone_turn("lokode-turn:0 undo of turn 3"), Some(3));
        assert_eq!(undone_turn("lokode-turn:2 write_file"), None);
    }
}
