//! A copy of the project for an agent that changes files in the background.
//!
//! The agent works in a git worktree of its own, outside the project, made from the project
//! as it stood, files not committed or not tracked included. Its shell may write only there.
//! Nothing it does reaches the project until the session brings its work in with
//! `take_work`, and the session is made to look first:
//!
//! | Since the agent started, the project | `take_work` |
//! |---|---|
//! | changed nothing | brings the work in |
//! | changed files | writes nothing and shows what changed, both sides of every file both touched |
//! | changed files, and the session wrote those both touched with `merged` | brings in the rest |
//!
//! The session thus reconciles what both sides meant itself, and the whole stays coherent.
//! The worktree goes once its work is in, and stays, with its path said, when it cannot be.
//!
//! A project outside git is copied through a private repository kept in the same place, which
//! holds where the agent started and goes with the copy. A project too large to copy, or one
//! its repository does not track, has no copy: the agent writes in the project, holding what
//! it writes until it answers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::tools::registry::{Tool, ToolCtx, ToolOutput};

/// Characters of a patch or a review the session reads before deciding.
const SHOWN_PATCH: usize = 30_000;

/// An agent's copy of the project.
#[derive(Debug, Clone)]
pub struct Work {
    pub id: u64,
    pub agent: String,
    pub task: String,
    /// The top of the copy, a worktree of the project's repository.
    pub dir: PathBuf,
    /// The project's directory inside the copy, where the agent works.
    pub root: PathBuf,
    /// The top of the project, where the work comes in: its repository's top, or the
    /// project itself when it is not in git.
    top: PathBuf,
    /// The private repository kept for a project outside git, which holds where the agent
    /// started. `None` when the project's own repository does.
    git_dir: Option<PathBuf>,
    /// The commit the copy started from.
    base: String,
    /// How many of the session's questions had been asked when the agent started: those
    /// after are what the project changed for meanwhile.
    pub asked_from: usize,
    /// What the agent said it did.
    pub answer: String,
    /// The project as the session was shown it, once it was: the side its proof answers.
    reviewed: Option<String>,
}

/// Opens what `take_work` says when the proofs do not account for the files.
pub const REFUSED: &str = "Nothing was written: the proofs do not account for";

/// Bytes of a project outside git a copy may hold. A larger project is worked in directly,
/// its files held for the agent, rather than copied.
const MAX_COPY_BYTES: u64 = 256 * 1024 * 1024;

/// Characters of an objective shown in a review: the agent's answer, a question asked.
const SHOWN_GOAL: usize = 600;

/// The latest questions of the session shown as the project's objectives.
const SHOWN_ASKED: usize = 3;

/// Characters of one side's changes the coherence check reads.
const SHOWN_EVIDENCE: usize = 6000;

/// Tokens the coherence check may answer with.
const CHECK_TOKENS: u32 = 250;

/// Lines of a refused proof listed before counting the rest.
const SHOWN_UNPROVEN: usize = 20;

/// Why a file both sides changed is as it is now, for one file.
#[derive(Debug, Clone, Deserialize)]
pub struct Proof {
    pub path: String,
    /// What the agent meant in this file.
    pub agent_goal: String,
    /// What the project's changes meant in this file.
    pub project_goal: String,
    /// How the file now serves both.
    pub how: String,
    /// The lines one side changed that the file leaves out on purpose, and why.
    #[serde(default)]
    pub dropped: Vec<Dropped>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Dropped {
    pub line: String,
    pub why: String,
}

/// Whether a line says too little to be followed: blank, or punctuation alone.
fn trivial(line: &str) -> bool {
    let t = line.trim();
    t.chars().filter(|c| c.is_alphanumeric()).count() < 2
}

/// The lines `side` added to and removed from `base`, trimmed, the trivial left out.
fn changes(base: &str, side: &str) -> (Vec<String>, Vec<String>) {
    let (mut added, mut removed) = (Vec::new(), Vec::new());
    for change in similar::TextDiff::from_lines(base, side).iter_all_changes() {
        let line = change.value().trim().to_string();
        if trivial(&line) {
            continue;
        }
        match change.tag() {
            similar::ChangeTag::Insert => added.push(line),
            similar::ChangeTag::Delete => removed.push(line),
            similar::ChangeTag::Equal => {}
        }
    }
    (added, removed)
}

/// What `result` fails to keep of what the agent and the project each changed from
/// `base`, and that `dropped` does not account for: a line one side added that is missing,
/// a line one side removed that is still there.
pub fn unproven(
    base: &str,
    agent: &str,
    project: &str,
    result: &str,
    dropped: &[Dropped],
) -> Vec<String> {
    let count = |text: &str, line: &str| text.lines().filter(|l| l.trim() == line).count();
    let accounted = |line: &str| {
        dropped
            .iter()
            .any(|d| d.line.trim() == line && !d.why.trim().is_empty())
    };
    let (agent_added, agent_removed) = changes(base, agent);
    let (project_added, project_removed) = changes(base, project);
    let mut out = Vec::new();
    for (who, added, other_added) in [
        ("the agent", &agent_added, &project_added),
        ("the project", &project_added, &agent_added),
    ] {
        for line in added {
            if count(result, line) == 0 && !accounted(line) {
                out.push(format!("{who} added it, the file lacks it: {line}"));
            }
        }
        let removed = match who {
            "the agent" => &agent_removed,
            _ => &project_removed,
        };
        for line in removed {
            if other_added.contains(line) || accounted(line) {
                continue;
            }
            if count(result, line) >= count(base, line) {
                out.push(format!("{who} removed it, the file still has it: {line}"));
            }
        }
    }
    out.dedup();
    out
}

/// The work agents did in their copies, waiting for the session to bring it in.
pub type Pending = Arc<std::sync::Mutex<HashMap<u64, Work>>>;

async fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = git_raw(dir, args, &[]).await?;
    Ok(String::from_utf8_lossy(&out).trim_end().to_string())
}

/// Run git in `dir` with `args` and `envs`, and give what it printed as it printed it.
async fn git_raw(dir: &Path, args: &[&str], envs: &[(&str, &Path)]) -> Result<Vec<u8>> {
    let out = tokio::process::Command::new("git")
        .args(args)
        .envs(envs.iter().map(|(k, v)| (*k, *v)))
        .current_dir(dir)
        // The commit that holds what is not committed is lokode's, and needs no identity of
        // the user's.
        .env("GIT_AUTHOR_NAME", "lokode")
        .env("GIT_AUTHOR_EMAIL", "lokode@localhost")
        .env("GIT_COMMITTER_NAME", "lokode")
        .env("GIT_COMMITTER_EMAIL", "lokode@localhost")
        .stdin(std::process::Stdio::null())
        .output()
        .await?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.first().copied().unwrap_or_default(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

/// A commit of the project at `top` as it stands, files not committed and files git does
/// not track included, its ignored files left out. Made with an index of its own in
/// `home`, so the project's branch, index and stash are not touched.
async fn snapshot(top: &Path, home: &Path, id: u64, repo: &[(&str, &Path)]) -> Result<String> {
    let index = home.join(format!("index-{id}-{}", std::process::id()));
    let mut envs: Vec<(&str, &Path)> = repo.to_vec();
    envs.push(("GIT_INDEX_FILE", index.as_path()));
    let envs = envs.as_slice();
    let head = git_raw(top, &["rev-parse", "--verify", "--quiet", "HEAD"], repo)
        .await
        .ok()
        .map(|b| String::from_utf8_lossy(&b).trim().to_string())
        .filter(|h| !h.is_empty());
    let made = async {
        if let Some(head) = &head {
            git_raw(top, &["read-tree", head], envs).await?;
        }
        git_raw(top, &["add", "--all"], envs).await?;
        let tree = String::from_utf8_lossy(&git_raw(top, &["write-tree"], envs).await?)
            .trim()
            .to_string();
        let mut args = vec![
            "commit-tree",
            tree.as_str(),
            "-m",
            "lokode: the project as an agent found it",
        ];
        if let Some(head) = &head {
            args.extend(["-p", head.as_str()]);
        }
        let commit = git_raw(top, &args, envs).await?;
        anyhow::Ok(String::from_utf8_lossy(&commit).trim().to_string())
    }
    .await;
    let _ = std::fs::remove_file(&index);
    made
}

/// Where the copies are made: beside lokode's state, never in the project.
pub fn home() -> Option<PathBuf> {
    crate::paths::state_dir().map(|d| d.join("worktrees"))
}

/// What the project and the agent changed since the agent started.
#[derive(Debug, Default)]
pub struct Sides {
    /// The files the project changed.
    pub project: Vec<String>,
    /// The files the agent changed.
    pub agent: Vec<String>,
    /// The commit of the project as it stands.
    now: String,
    /// The commit of the agent's copy as it stands.
    theirs: String,
}

impl Sides {
    /// The files both changed.
    pub fn both(&self) -> Vec<String> {
        self.agent
            .iter()
            .filter(|f| self.project.contains(f))
            .cloned()
            .collect()
    }
}

/// What `take_work` did.
#[derive(Debug)]
pub enum Taken {
    /// The work is in the project: the files it changed, and what the coherence check is
    /// given when the project changed meanwhile.
    In {
        files: Vec<String>,
        evidence: Option<String>,
    },
    /// Nothing was written: what the session has to look at first.
    Review(String),
    /// Nothing was written: what the proofs do not account for.
    Refused(Vec<String>),
}

impl Work {
    /// A copy, made in `home`, of the project at `root` for the agent `agent`, numbered
    /// `id`, as the project stands now. A project outside git has a private repository kept
    /// for it in `home`, which holds where the agent started. `None` when the project is
    /// too large to copy.
    pub async fn make(
        home: &Path,
        root: &Path,
        id: u64,
        agent: &str,
        task: &str,
    ) -> Result<Option<Work>> {
        std::fs::create_dir_all(home)?;
        let tag = format!("{agent}-{id}-{}", std::process::id());
        let (top, git_dir) = match git(root, &["rev-parse", "--show-toplevel"]).await {
            Ok(top) => (PathBuf::from(top), None),
            Err(_) => {
                let repo = home.join(format!("{tag}.git"));
                git_raw(
                    home,
                    &["init", "--quiet", "--bare"],
                    &[("GIT_DIR", repo.as_path())],
                )
                .await?;
                (root.to_path_buf(), Some(repo))
            }
        };
        let mut work = Work {
            id,
            agent: agent.to_string(),
            task: task.to_string(),
            root: PathBuf::new(),
            dir: home.join(&tag),
            top,
            git_dir,
            base: String::new(),
            asked_from: 0,
            answer: String::new(),
            reviewed: None,
        };
        work.base = snapshot(&work.top, home, id, &work.repo_env()).await?;
        if work.git_dir.is_some() && work.size().await > MAX_COPY_BYTES {
            work.remove().await;
            return Ok(None);
        }
        let shown = work.dir.display().to_string();
        // Only the repository's own directory: a work tree named here would make git take
        // the project for the main checkout of a repository it is not.
        let envs: Vec<(&str, &Path)> = work
            .git_dir
            .iter()
            .map(|g| ("GIT_DIR", g.as_path()))
            .collect();
        git_raw(
            &work.top,
            &["worktree", "add", "--quiet", "--detach", &shown, &work.base],
            &envs,
        )
        .await?;
        let inside = root.strip_prefix(&work.top).unwrap_or(Path::new(""));
        work.root = work.dir.join(inside);
        // A project its repository does not track, an ignored directory of an enclosing one,
        // is not in the copy: the agent then writes in the project, holding what it writes.
        if !work.root.is_dir() {
            work.remove().await;
            return Ok(None);
        }
        Ok(Some(work))
    }

    /// The environment that points git at the repository holding where the agent started.
    fn repo_env(&self) -> Vec<(&'static str, &Path)> {
        match &self.git_dir {
            Some(dir) => vec![
                ("GIT_DIR", dir.as_path()),
                ("GIT_WORK_TREE", self.top.as_path()),
            ],
            None => Vec::new(),
        }
    }

    /// Git in the project's side: its repository, or the private one for a project outside
    /// git.
    async fn pg(&self, args: &[&str]) -> Result<String> {
        let out = git_raw(&self.top, args, &self.repo_env()).await?;
        Ok(String::from_utf8_lossy(&out).trim_end().to_string())
    }

    /// The bytes the project's files take, as the copy would hold them.
    async fn size(&self) -> u64 {
        self.pg(&["ls-tree", "-r", "-l", &self.base])
            .await
            .map(|out| {
                out.lines()
                    .filter_map(|l| l.split_whitespace().nth(3)?.parse::<u64>().ok())
                    .sum()
            })
            .unwrap_or(0)
    }

    /// The agent's copy as it stands, as a commit on where it started.
    async fn theirs(&self) -> Result<String> {
        git(&self.dir, &["add", "--all"]).await?;
        let tree = git(&self.dir, &["write-tree"]).await?;
        git(
            &self.dir,
            &[
                "commit-tree",
                &tree,
                "-p",
                &self.base,
                "-m",
                "lokode: what the agent did",
            ],
        )
        .await
    }

    /// The files changed between 2 commits.
    async fn changed(&self, from: &str, to: &str) -> Result<Vec<String>> {
        Ok(self
            .pg(&["diff", "--name-only", "--no-renames", from, to])
            .await?
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect())
    }

    /// What the project and the agent changed since the agent started.
    pub async fn sides(&self) -> Result<Sides> {
        let home = self.dir.parent().unwrap_or(&self.dir).to_path_buf();
        let now = snapshot(&self.top, &home, self.id, &self.repo_env()).await?;
        let theirs = self.theirs().await?;
        Ok(Sides {
            project: self.changed(&self.base, &now).await?,
            agent: self.changed(&self.base, &theirs).await?,
            now,
            theirs,
        })
    }

    /// Everything the agent changed in its copy, as one patch against where it started.
    pub async fn patch(&self) -> Result<String> {
        let theirs = self.theirs().await?;
        let mut patch = self
            .pg(&["diff", "-U1", "--no-renames", &self.base, &theirs])
            .await?;
        if !patch.is_empty() {
            patch.push('\n');
        }
        Ok(patch)
    }

    /// The text of `path` at `commit`, empty when it is not there.
    async fn at(&self, commit: &str, path: &str) -> String {
        // As stored, its last newline kept: a comparison by lines depends on it.
        git_raw(
            &self.top,
            &["show", &format!("{commit}:{path}")],
            &self.repo_env(),
        )
        .await
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
    }

    /// Bring the work in, or say what the session must do first. `proofs` say how each
    /// file both sides changed was reconciled; `None` until the session looked. `asked`
    /// are the session's questions, for the project's objectives.
    pub async fn take(
        &mut self,
        proofs: Option<&[Proof]>,
        asked: &[String],
        max_chars: usize,
    ) -> Result<Taken> {
        let sides = self.sides().await?;
        // Nothing changed meanwhile: the agent's work starts from the project as it is.
        if sides.project.is_empty() {
            return Ok(Taken::In {
                files: self.copy_in(&sides.agent, &[])?,
                evidence: None,
            });
        }
        let both = sides.both();
        let (Some(proofs), Some(reviewed)) = (proofs, self.reviewed.clone()) else {
            self.reviewed = Some(sides.now.clone());
            return Ok(Taken::Review(
                self.review(&sides, &both, asked, max_chars).await,
            ));
        };
        let mut refused = Vec::new();
        for path in &both {
            let Some(proof) = proofs.iter().find(|p| &p.path == path) else {
                refused.push(format!("{path}: no proof"));
                continue;
            };
            if [&proof.agent_goal, &proof.project_goal, &proof.how]
                .iter()
                .any(|t| t.trim().is_empty())
            {
                refused.push(format!("{path}: the proof leaves a goal or the how empty"));
                continue;
            }
            let base = self.at(&self.base, path).await;
            let agent = std::fs::read_to_string(self.dir.join(path)).unwrap_or_default();
            let project = self.at(&reviewed, path).await;
            let result = std::fs::read_to_string(self.top.join(path)).unwrap_or_default();
            let lacking = unproven(&base, &agent, &project, &result, &proof.dropped);
            let shown = lacking.len().min(SHOWN_UNPROVEN);
            for line in &lacking[..shown] {
                refused.push(format!("{path}: {line}"));
            }
            if lacking.len() > shown {
                refused.push(format!("{path}: and {} more", lacking.len() - shown));
            }
        }
        if !refused.is_empty() {
            return Ok(Taken::Refused(refused));
        }
        // Taken before the agent's own files come in: both sides' diffs are what is checked.
        let evidence = self.evidence(&sides, asked).await;
        let mut done = self.copy_in(&sides.agent, &both)?;
        done.extend(both);
        Ok(Taken::In {
            files: done,
            evidence: Some(evidence),
        })
    }

    /// Copy into the project the files of `agent` but those in `skip`, the session having
    /// written those itself. Gives the files copied.
    fn copy_in(&self, agent: &[String], skip: &[String]) -> Result<Vec<String>> {
        let mut brought = Vec::new();
        for path in agent.iter().filter(|f| !skip.contains(f)) {
            let at = self.top.join(path);
            match std::fs::read(self.dir.join(path)) {
                Ok(bytes) => {
                    if let Some(dir) = at.parent() {
                        std::fs::create_dir_all(dir)?;
                    }
                    std::fs::write(&at, bytes)?;
                }
                Err(_) => {
                    let _ = std::fs::remove_file(&at);
                }
            }
            brought.push(path.clone());
        }
        Ok(brought)
    }

    /// Both objectives, the agent's task with what it said it did and the last questions
    /// asked since, each cut short.
    fn objectives(&self, asked: &[String]) -> String {
        let short = |t: &str| t.chars().take(SHOWN_GOAL).collect::<String>();
        let since: Vec<String> = asked
            .iter()
            .skip(self.asked_from)
            .rev()
            .take(SHOWN_ASKED)
            .map(|q| format!("\"{}\"", short(q).replace('\n', " ")))
            .collect();
        format!(
            "Objectives:\n- the agent: {task}. It said: {answer}\n- the project since: {since}\n",
            task = self.task,
            answer = short(&self.answer),
            since = match since.is_empty() {
                true => "changes made outside this session".to_string(),
                false => since.join("; "),
            },
        )
    }

    /// What the coherence check is given: both objectives and the 2 sides' changes since
    /// the agent started, without their context lines and cut short.
    async fn evidence(&self, sides: &Sides, asked: &[String]) -> String {
        // Without git's own headers, which say nothing the file names and hunks do not.
        let cut = |t: String| {
            t.lines()
                .filter(|l| !l.starts_with("diff --git") && !l.starts_with("index "))
                .collect::<Vec<_>>()
                .join("\n")
                .chars()
                .take(SHOWN_EVIDENCE)
                .collect::<String>()
        };
        let project = self
            .pg(&["diff", "-U0", "--no-renames", &self.base, &sides.now])
            .await
            .map(cut)
            .unwrap_or_default();
        let agent = self
            .pg(&["diff", "-U0", "--no-renames", &self.base, &sides.theirs])
            .await
            .map(cut)
            .unwrap_or_default();
        format!(
            "{}\nWhat the project changed:\n```diff\n{project}\n```\nWhat the agent changed:\n```diff\n{agent}\n```",
            self.objectives(asked)
        )
    }

    /// What the session reads before bringing the work in: both objectives, what the
    /// project changed since the agent started, and both sides of every file both changed.
    /// The agent's own patch was in its answer, and is not repeated.
    async fn review(
        &self,
        sides: &Sides,
        both: &[String],
        asked: &[String],
        max_chars: usize,
    ) -> String {
        let stat = self
            .pg(&["diff", "--stat", "--no-renames", &self.base, &sides.now])
            .await
            .unwrap_or_default();
        let mut out = format!(
            "Nothing was written: the project changed since the agent {agent} started.\n{stat}\n{goals}",
            agent = self.agent,
            goals = self.objectives(asked),
        );
        let fits =
            "Check that the rest of the agent's work fits what the project changed, names and \
                    signatures it uses included.";
        if both.is_empty() {
            out.push_str(&format!(
                "\nNo file was changed by both. {fits} Then call take_work again with merged [].\n"
            ));
        } else {
            out.push_str(&format!(
                "\nBoth changed {}. Write each in the project so it serves both objectives. {fits} \
                 Then call take_work again with merged: for each file, path, agent_goal, \
                 project_goal, how, and dropped for any line either side added or removed that the \
                 file leaves as it was, with why. Every other such line is checked.\n",
                both.join(", ")
            ));
            for path in both {
                let ours = self
                    .pg(&["diff", "-U1", &self.base, &sides.now, "--", path])
                    .await
                    .unwrap_or_default();
                let theirs = self
                    .pg(&["diff", "-U1", &self.base, &sides.theirs, "--", path])
                    .await
                    .unwrap_or_default();
                out.push_str(&format!(
                    "\n{path}, the project:\n```diff\n{ours}\n```\n{path}, the agent:\n```diff\n{theirs}\n```\n"
                ));
            }
        }
        out.chars().take(SHOWN_PATCH.min(max_chars)).collect()
    }

    /// Take the copy away.
    pub async fn remove(&self) {
        let shown = self.dir.display().to_string();
        // Naming the repository alone, as when the copy was made.
        let envs: Vec<(&str, &Path)> = self
            .git_dir
            .iter()
            .map(|g| ("GIT_DIR", g.as_path()))
            .collect();
        if git_raw(&self.top, &["worktree", "remove", "--force", &shown], &envs)
            .await
            .is_err()
        {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
        let _ = git_raw(&self.top, &["worktree", "prune"], &envs).await;
        if let Some(repo) = &self.git_dir {
            let _ = std::fs::remove_dir_all(repo);
        }
    }

    /// What the session is told once the agent answered: its answer, what it changed and
    /// how to bring it in.
    pub async fn report(&self, answer: &str, max_chars: usize) -> String {
        let patch = self.patch().await.unwrap_or_default();
        let files = self.sides().await.map(|s| s.agent).unwrap_or_default();
        let shown: String = patch.chars().take(SHOWN_PATCH.min(max_chars)).collect();
        let cut = match shown.len() < patch.len() {
            true => "\n(the patch goes on; read the files in the copy for the rest)",
            false => "",
        };
        format!(
            "The agent {agent}, started in the background on \"{task}\", answered:\n{answer}\n\n\
             It changed {n} file(s) in its own copy of the project, {dir}: {files}. Nothing \
             reached the project yet. Read the patch, then call take_work with work {id} to \
             bring it in.\n```diff\n{shown}```{cut}",
            agent = self.agent,
            task = self.task,
            n = files.len(),
            dir = self.dir.display(),
            files = files.join(", "),
            id = self.id,
        )
    }
}

/// Who checks that 2 changes made apart fit together once they are in one project: a
/// model asked one question about the 2 changes, with nothing else of the session.
#[derive(Clone)]
pub struct Reviewer {
    pub backend: Arc<dyn crate::backend::provider::Provider>,
    /// The model of the `review` role, or the session's.
    pub model: String,
    pub session: String,
}

impl Reviewer {
    /// What does not fit between the 2 changes in `evidence`, a line a problem, or `None`
    /// when they fit or the check could not be made.
    pub async fn check(
        &self,
        evidence: &str,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Option<String> {
        use crate::backend::wire::Message;
        let req = crate::backend::provider::TurnRequest {
            model: self.model.clone(),
            messages: vec![
                Message::system(
                    "Two changes were made to one code base apart from each other and are now \
                     both in it. Answer with the single word OK when they fit together. \
                     Otherwise answer with up to five lines, each `file: problem`, naming only \
                     what would not work together: a name, signature or type one change uses \
                     and the other renamed, changed or removed; the same thing done twice; \
                     behaviour that contradicts; something one change needed that the other \
                     removed. Do not comment on style and do not suggest improvements.",
                ),
                Message::user(evidence.to_string()),
            ],
            tools: None,
            grammar: None,
            max_tokens: CHECK_TOKENS,
            temperature: 0.0,
            user: Some(format!("{}-coherence", self.session)),
            reason: false,
            purpose: crate::backend::meter::Purpose::Review,
        };
        let answer = tokio::select! {
            biased;
            _ = cancel.cancelled() => return None,
            r = self.backend.complete(req) => r.ok()?,
        };
        let answer = answer.trim();
        let head = answer.trim_start_matches(['"', '`', '*']).trim_start();
        if head.is_empty() || head.to_lowercase().starts_with("ok") {
            return None;
        }
        Some(head.lines().take(5).collect::<Vec<_>>().join("\n"))
    }
}

/// The tool that brings an agent's work into the project.
pub struct TakeWork {
    pending: Pending,
    reviewer: Option<Reviewer>,
}

impl TakeWork {
    pub fn new(pending: Pending, reviewer: Option<Reviewer>) -> Self {
        Self { pending, reviewer }
    }
}

#[derive(Deserialize)]
struct Args {
    work: u64,
    #[serde(default)]
    merged: Option<Vec<Proof>>,
}

#[async_trait]
impl Tool for TakeWork {
    fn name(&self) -> &str {
        "take_work"
    }

    fn description(&self) -> &str {
        "Bring into the project the work an agent did in the background in its own copy, \
         named by the number its answer gave. When the project changed since the agent \
         started, nothing is written: both objectives and what changed are shown, you write \
         the files both changed so each serves both, then call again with merged, a proof \
         for each. The result is checked, and a failure is yours to fix."
    }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "work": {"type": "integer", "minimum": 1},
                "merged": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string"},
                            "agent_goal": {"type": "string"},
                            "project_goal": {"type": "string"},
                            "how": {"type": "string"},
                            "dropped": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "line": {"type": "string"},
                                        "why": {"type": "string"}
                                    },
                                    "required": ["line", "why"],
                                    "additionalProperties": false
                                }
                            }
                        },
                        "required": ["path", "agent_goal", "project_goal", "how"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["work"],
            "additionalProperties": false
        })
    }

    fn mutates(&self) -> bool {
        true
    }

    async fn run(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput> {
        let a: Args = serde_json::from_value(args)?;
        let work = self
            .pending
            .lock()
            .ok()
            .and_then(|p| p.get(&a.work).cloned());
        let Some(mut work) = work else {
            return Ok(ToolOutput::err(format!(
                "there is no work {} waiting to be brought in",
                a.work
            )));
        };
        let asked = ctx.asked.lock().map(|a| a.clone()).unwrap_or_default();
        let taken = work
            .take(a.merged.as_deref(), &asked, ctx.max_output_chars)
            .await;
        // What the session was shown is kept with the work, for its proof to answer.
        if let Ok(mut p) = self.pending.lock() {
            if let Some(slot) = p.get_mut(&a.work) {
                *slot = work.clone();
            }
        }
        match taken {
            Ok(Taken::Review(review)) => Ok(ToolOutput::ok(review)),
            Ok(Taken::Refused(lacking)) => Ok(ToolOutput::err(format!(
                "{REFUSED}\n{}\nFix the files or say in dropped why, then call take_work \
                 again.",
                lacking.join("\n")
            ))),
            Ok(Taken::In { files, evidence }) => {
                // Both changes are in the project: a model reads them together, once, when
                // the project changed meanwhile.
                let objection = match (&self.reviewer, &evidence) {
                    (Some(reviewer), Some(evidence)) => reviewer.check(evidence, &ctx.cancel).await,
                    _ => None,
                };
                if let Ok(mut p) = self.pending.lock() {
                    p.remove(&a.work);
                }
                work.remove().await;
                // One line a file, as a patch says it, so the turn counts and checks it.
                let mut out = format!("brought in the work of {}:", work.agent);
                for f in &files {
                    out.push_str(&format!("\npatched {f}"));
                }
                for proof in a.merged.iter().flatten() {
                    out.push_str(&format!("\nreconciled {}: {}", proof.path, proof.how));
                }
                if files.is_empty() {
                    out.push_str("\nnothing more to bring in");
                }
                out.push_str(
                    "\nThe files are now checked: an error reported below comes from putting the \
                     two together, and is yours to fix before the turn ends.",
                );
                if let Some(objection) = objection {
                    out.push_str(&format!(
                        "\n\nThe coherence check says these do not fit together:\n{objection}\n\
                         Fix each in the project's files now, so the whole works, before the turn ends."
                    ));
                }
                Ok(ToolOutput::ok(out))
            }
            Err(e) => Ok(ToolOutput::err(format!(
                "the work of {} could not be brought in: {e:#}. It stays in {}.",
                work.agent,
                work.dir.display()
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    async fn take(tool: &TakeWork, args: Value) -> ToolOutput {
        let h = crate::tools::harness::Harness::new(crate::tools::harness::Answer::Yes);
        tool.run(&h.ctx, args).await.unwrap()
    }

    /// A project outside git is copied through a private repository, its ignored files left
    /// out, its work reconciled as in git, and the private repository goes with the copy.
    #[tokio::test]
    async fn a_project_outside_git_is_copied_and_taken_back() {
        let state = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let p = project.path();
        std::fs::write(p.join(".gitignore"), "*.tmp\n").unwrap();
        std::fs::write(p.join("a.txt"), "one\n").unwrap();
        std::fs::write(p.join("skip.tmp"), "ignored\n").unwrap();
        assert!(!p.join(".git").exists());

        let work = Work::make(state.path(), p, 3, "scribe", "write b")
            .await
            .unwrap()
            .expect("a small project outside git is copied");
        assert_eq!(
            std::fs::read_to_string(work.root.join("a.txt")).unwrap(),
            "one\n"
        );
        assert!(!work.root.join("skip.tmp").exists());
        assert!(!p.join(".git").exists(), "the project became a repository");
        std::fs::write(work.root.join("b.txt"), "new\n").unwrap();
        assert!(!p.join("b.txt").exists());

        let pending: Pending = Default::default();
        pending.lock().unwrap().insert(3, work.clone());
        let tool = TakeWork::new(pending, None);
        let out = take(&tool, json!({"work": 3})).await;
        assert!(out.content.contains("patched b.txt"), "{}", out.content);
        assert_eq!(std::fs::read_to_string(p.join("b.txt")).unwrap(), "new\n");
        assert!(!work.dir.exists());
        assert!(
            std::fs::read_dir(state.path()).unwrap().next().is_none(),
            "the private repository stayed"
        );

        // The project changed meanwhile: the same review as in git.
        let work = Work::make(state.path(), p, 4, "scribe", "edit a")
            .await
            .unwrap()
            .unwrap();
        std::fs::write(work.root.join("a.txt"), "one\ntheirs\n").unwrap();
        std::fs::write(p.join("a.txt"), "ONE\n").unwrap();
        let pending: Pending = Default::default();
        pending.lock().unwrap().insert(4, work);
        let review = take(&TakeWork::new(pending, None), json!({"work": 4})).await;
        assert!(
            review.content.contains("Both changed a.txt"),
            "{}",
            review.content
        );
    }

    /// A project too large to copy is not copied.
    #[tokio::test]
    async fn a_project_too_large_is_not_copied() {
        let state = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let big = vec![b'x'; (MAX_COPY_BYTES / 4 + 1) as usize];
        for n in 0..5 {
            std::fs::write(project.path().join(format!("f{n}")), &big).unwrap();
        }
        let none = Work::make(state.path(), project.path(), 1, "scribe", "t")
            .await
            .unwrap();
        assert!(none.is_none());
        assert!(std::fs::read_dir(state.path()).unwrap().next().is_none());
    }

    /// A copy starts from the project as it stands, files not committed or not tracked
    /// included, and its work comes in at once when the project did not change since.
    #[tokio::test]
    async fn work_comes_in_when_nothing_changed_meanwhile() {
        let state = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let p = project.path();
        run(p, &["init", "-q"]);
        std::fs::write(p.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(p.join("a.txt"), "one\n").unwrap();
        run(p, &["add", "-A"]);
        run(p, &["commit", "-qm", "one"]);
        std::fs::write(p.join("a.txt"), "one\nnot committed\n").unwrap();
        std::fs::write(p.join("loose.txt"), "not tracked\n").unwrap();
        std::fs::create_dir_all(p.join("target")).unwrap();
        std::fs::write(p.join("target/built"), "ignored").unwrap();

        let work = Work::make(state.path(), p, 7, "scribe", "write b")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(work.root.join("a.txt")).unwrap(),
            "one\nnot committed\n"
        );
        assert_eq!(
            std::fs::read_to_string(work.root.join("loose.txt")).unwrap(),
            "not tracked\n"
        );
        assert!(!work.root.join("target/built").exists());
        let staged = std::process::Command::new("git")
            .args(["diff", "--cached", "--name-only"])
            .current_dir(p)
            .output()
            .unwrap();
        assert!(staged.stdout.is_empty(), "the project's index was touched");

        std::fs::write(work.root.join("b.txt"), "new\n").unwrap();
        assert!(!p.join("b.txt").exists());
        let report = work.report("done", 100_000).await;
        assert!(
            report.contains("b.txt") && report.contains("work 7"),
            "{report}"
        );
        assert!(
            !report.contains("loose.txt"),
            "an untouched file is not work: {report}"
        );

        let pending: Pending = Default::default();
        pending.lock().unwrap().insert(7, work.clone());
        let tool = TakeWork::new(pending, None);
        let out = take(&tool, json!({"work": 7})).await;
        assert!(out.content.contains("patched b.txt"), "{}", out.content);
        assert_eq!(std::fs::read_to_string(p.join("b.txt")).unwrap(), "new\n");
        assert!(!work.dir.exists(), "the copy stayed after its work came in");
    }

    /// When the project changed meanwhile nothing is written until the session looked,
    /// and a file both changed comes in only with a proof the file bears out.
    #[tokio::test]
    async fn the_session_proves_what_it_reconciled() {
        let state = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let p = project.path();
        run(p, &["init", "-q"]);
        std::fs::write(p.join("a.txt"), "one\n").unwrap();
        std::fs::write(p.join("c.txt"), "c\n").unwrap();
        run(p, &["add", "-A"]);
        run(p, &["commit", "-qm", "one"]);
        let mut work = Work::make(state.path(), p, 8, "scribe", "sign a")
            .await
            .unwrap()
            .unwrap();
        work.answer = "signed a.txt and added b.txt".into();
        work.asked_from = 1;
        std::fs::write(work.root.join("a.txt"), "one\nsigned by the scribe\n").unwrap();
        std::fs::write(work.root.join("b.txt"), "b file\n").unwrap();
        std::fs::write(p.join("a.txt"), "ONE upper\n").unwrap();
        std::fs::write(p.join("c.txt"), "C\n").unwrap();

        let pending: Pending = Default::default();
        pending.lock().unwrap().insert(8, work.clone());
        let tool = TakeWork::new(pending, None);
        let h = crate::tools::harness::Harness::new(crate::tools::harness::Answer::Yes);
        *h.ctx.asked.lock().unwrap() = vec!["first".into(), "make a.txt upper case".into()];
        let call = |args: Value| {
            let (tool, ctx) = (&tool, &h.ctx);
            async move { tool.run(ctx, args).await.unwrap() }
        };
        let review = call(json!({"work": 8})).await;
        assert!(
            review.content.starts_with("Nothing was written"),
            "{}",
            review.content
        );
        assert!(
            review.content.contains("Both changed a.txt"),
            "{}",
            review.content
        );
        assert!(
            review.content.contains("make a.txt upper case"),
            "{}",
            review.content
        );
        assert!(!review.content.contains("\"first\""), "{}", review.content);
        assert!(
            review.content.contains("signed a.txt"),
            "{}",
            review.content
        );
        assert!(!p.join("b.txt").exists());

        let proof = |how: &str| {
            json!({"work": 8, "merged": [{"path": "a.txt", "agent_goal": "sign it",
                "project_goal": "upper case", "how": how}]})
        };
        // No proof, then a file that drops the agent's line: both refused.
        let none = call(json!({"work": 8, "merged": []})).await;
        assert!(
            none.is_error && none.content.contains("a.txt: no proof"),
            "{}",
            none.content
        );
        std::fs::write(p.join("a.txt"), "ONE upper\n").unwrap();
        let short = call(proof("kept the upper case")).await;
        assert!(short.is_error, "{}", short.content);
        assert!(
            short
                .content
                .contains("the file lacks it: signed by the scribe"),
            "{}",
            short.content
        );
        assert!(!p.join("b.txt").exists());

        std::fs::write(p.join("a.txt"), "ONE upper\nsigned by the scribe\n").unwrap();
        let out = call(proof("upper case first line, the signature after")).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("patched b.txt") && out.content.contains("patched a.txt"));
        assert!(
            out.content.contains("reconciled a.txt: upper case"),
            "{}",
            out.content
        );
        assert_eq!(
            std::fs::read_to_string(p.join("b.txt")).unwrap(),
            "b file\n"
        );
        assert_eq!(std::fs::read_to_string(p.join("c.txt")).unwrap(), "C\n");
        assert!(!work.dir.exists());
    }

    /// The coherence check is asked once, only when the project changed meanwhile, with the
    /// 2 sides and the objectives and nothing else, and its objection is given back.
    #[tokio::test]
    async fn the_coherence_check_reads_both_sides_once() {
        use crate::backend::wire::Message;
        struct Scripted(std::sync::Mutex<Vec<Vec<Message>>>, &'static str);
        #[async_trait]
        impl crate::backend::provider::Provider for Scripted {
            fn label(&self) -> &str {
                "scripted"
            }
            fn capabilities(&self) -> crate::backend::provider::Capabilities {
                crate::backend::provider::Capabilities {
                    native_tools: false,
                    grammar: false,
                    usage_in_stream: false,
                }
            }
            async fn stream(
                &self,
                _: crate::backend::provider::TurnRequest,
            ) -> Result<crate::backend::client::ChunkStream> {
                bail!("not used")
            }
            async fn complete(&self, req: crate::backend::provider::TurnRequest) -> Result<String> {
                self.0.lock().unwrap().push(req.messages);
                Ok(self.1.to_string())
            }
            async fn models(&self) -> Result<Vec<String>> {
                Ok(Vec::new())
            }
        }
        let seen = Arc::new(Scripted(
            Default::default(),
            "lib.rs: parse() was renamed read() in the project",
        ));
        let reviewer = Reviewer {
            backend: seen.clone(),
            model: "small".into(),
            session: "s".into(),
        };
        let state = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let p = project.path();
        run(p, &["init", "-q"]);
        std::fs::write(p.join("lib.rs"), "fn parse() {}\n").unwrap();
        run(p, &["add", "-A"]);
        run(p, &["commit", "-qm", "one"]);
        let mut work = Work::make(state.path(), p, 5, "scribe", "call parse in main")
            .await
            .unwrap()
            .unwrap();
        work.answer = "main calls parse".into();
        std::fs::write(work.root.join("main.rs"), "fn main() { parse(); }\n").unwrap();
        std::fs::write(p.join("lib.rs"), "fn read() {}\n").unwrap();
        let pending: Pending = Default::default();
        pending.lock().unwrap().insert(5, work);
        let tool = TakeWork::new(pending, Some(reviewer));
        let h = crate::tools::harness::Harness::new(crate::tools::harness::Answer::Yes);
        *h.ctx.asked.lock().unwrap() = vec!["rename parse to read".into()];
        // The first call only reviews. The second, with no file both changed, lands.
        let review = tool.run(&h.ctx, json!({"work": 5})).await.unwrap();
        assert!(
            review.content.starts_with("Nothing was written"),
            "{}",
            review.content
        );
        assert!(
            seen.0.lock().unwrap().is_empty(),
            "asked before anything was written"
        );
        let out = tool
            .run(&h.ctx, json!({"work": 5, "merged": []}))
            .await
            .unwrap();
        assert!(out.content.contains("patched main.rs"), "{}", out.content);
        assert!(
            out.content.contains("do not fit together") && out.content.contains("renamed read()"),
            "{}",
            out.content
        );
        let asked = seen.0.lock().unwrap();
        assert_eq!(asked.len(), 1);
        let text = &asked[0][1].content;
        assert!(
            text.contains("call parse in main") && text.contains("rename parse to read"),
            "{text}"
        );
        assert!(
            text.contains("-fn parse()") && text.contains("+fn main()"),
            "{text}"
        );
        assert!(
            !text.contains("\nindex "),
            "context lines were sent: {text}"
        );
    }

    /// A line one side added must be kept and a line one side removed must be gone, unless
    /// the proof says why; lines that say nothing are not followed.
    #[test]
    fn a_proof_accounts_for_every_line() {
        let base = "fn a() {\n    old();\n}\n";
        let agent = "fn a() {\n    old();\n    log();\n}\n";
        let project = "fn a() {\n    new();\n}\n";
        assert!(unproven(
            base,
            agent,
            project,
            "fn a() {\n    new();\n    log();\n}\n",
            &[]
        )
        .is_empty());
        let lacking = unproven(base, agent, project, "fn a() {\n    old();\n}\n", &[]);
        assert_eq!(lacking.len(), 3, "{lacking:?}");
        let why = [Dropped {
            line: "log();".into(),
            why: "the project logs already".into(),
        }];
        assert!(unproven(base, agent, project, "fn a() {\n    new();\n}\n", &why).is_empty());
    }
}
