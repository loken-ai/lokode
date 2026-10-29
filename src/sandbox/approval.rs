use std::collections::HashSet;
use std::path::{Path, PathBuf};

use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::{mpsc, oneshot, Mutex};

/// What an `Always` answer covers for the rest of the session.
///
/// Granted against what the user was shown, not against the tool that showed it. Keyed on
/// the tool's name it would mean that agreeing to one `ls` hands over a free shell, and
/// agreeing to one file being written hands over every file - which is not what the
/// question on screen appeared to ask.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalScope {
    /// One shell command, matched in full.
    ///
    /// The whole string, not the program at the front of it: `ls` and `ls; rm -rf ~`
    /// share a first word, so keying on the program would let the second inherit an
    /// agreement given to the first. Matching the whole command is narrow enough to be
    /// honest and still spares the user the repeats an agent actually produces, which are
    /// the same command run again.
    Command(String),
    /// One shell command, matched in full, run outside the sandbox.
    Unconfined(String),
    /// Writes by one tool anywhere under a directory.
    Path { tool: String, prefix: PathBuf },
    /// Network egress to one host.
    Host(String),
    /// Every call of a tool whose whole surface is the single action just approved.
    Tool(String),
}

impl ApprovalScope {
    /// How to describe this scope to the user, so `Always` is never wider than it reads.
    pub fn describe(&self) -> String {
        self.describe_under(std::path::Path::new(""))
    }

    /// The same, with paths written as the project sees them.
    ///
    /// A prompt is read in a pane a few dozen columns wide, and the part of a path that
    /// says what is about to change is the part below the project root.
    pub fn describe_under(&self, root: &std::path::Path) -> String {
        match self {
            ApprovalScope::Command(cmd) => format!("this exact command ({cmd})"),
            ApprovalScope::Unconfined(cmd) => {
                format!("this exact command outside the sandbox ({cmd})")
            }
            ApprovalScope::Path { tool, prefix } => {
                let shown = prefix.strip_prefix(root).unwrap_or(prefix).display();
                let shown = shown.to_string();
                if shown.is_empty() {
                    format!("{tool} anywhere in the project")
                } else {
                    format!("{tool} anywhere under {shown}")
                }
            }
            ApprovalScope::Host(host) => format!("requests to {host}"),
            ApprovalScope::Tool(tool) => format!("every {tool} call"),
        }
    }

    /// Whether an agreement already given covers this request.
    ///
    /// Only `Path` is not an equality test: an agreement given while looking at a file in
    /// a directory covers that directory's contents, which is the granularity the
    /// question was asked at.
    fn covered_by(&self, granted: &ApprovalScope) -> bool {
        match (granted, self) {
            (
                ApprovalScope::Path {
                    tool: gt,
                    prefix: gp,
                },
                ApprovalScope::Path {
                    tool: st,
                    prefix: sp,
                },
            ) => gt == st && sp.starts_with(gp),
            _ => granted == self,
        }
    }
}

/// Directory an `Always` answer about `target` should cover.
///
/// The file's parent: the user was shown one file, and the smallest honest reading of
/// "always" is the place that file lives in.
pub fn write_scope(tool: &str, target: &Path) -> ApprovalScope {
    ApprovalScope::Path {
        tool: tool.to_string(),
        prefix: target.parent().unwrap_or(target).to_path_buf(),
    }
}

/// What kind of approval is being requested.
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub tool: String,
    pub summary: String,
    /// Optional unified-diff preview (write/edit/patch tools).
    pub diff_preview: Option<String>,
    /// What answering `Always` would agree to from here on.
    pub scope: ApprovalScope,
}

/// Outcome of a single approval prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    Once,
    Always,
    /// Refused, with what the person said about it when they said anything.
    ///
    /// A bare refusal tells a model that the call failed, not that it was the wrong call,
    /// so the next step is usually the same call again, and the person refuses it twice.
    Deny(Option<String>),
}

/// What a tool should do once the question has been settled, one way or another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Approval {
    Proceed,
    Denied(Denial),
    /// The turn was abandoned while the prompt was on screen. Not the same as `Denied`:
    /// the user refused nothing, they stopped waiting.
    Interrupted,
}

/// Who refused a call, and what they said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Denial {
    /// The person, with what they said when they said anything.
    User(Option<String>),
    /// A rule of the configuration, named.
    Rule(String),
}

/// What a tool hands back when its call was refused.
///
/// Carries who refused it and why, so the model has something to act on rather than a
/// failure to work around.
pub fn denial(why: Denial) -> String {
    match why {
        Denial::User(None) => "user denied".to_string(),
        Denial::User(Some(why)) => format!("user denied: {why}"),
        Denial::Rule(why) => format!("{why}, without asking the user"),
    }
}

#[derive(Debug)]
pub struct ApprovalPrompt {
    pub req: ApprovalRequest,
    pub reply: oneshot::Sender<ApprovalDecision>,
}

/// Sent from the agent loop to the UI to ask for approval.
pub type ApprovalSender = mpsc::Sender<ApprovalPrompt>;
/// Received by the UI to display the modal.
pub type ApprovalReceiver = mpsc::Receiver<ApprovalPrompt>;

pub fn channel() -> (ApprovalSender, ApprovalReceiver) {
    mpsc::channel(8)
}

/// Tracks the `Always` agreements given during one session, and those kept for its
/// project when it keeps them.
#[derive(Default, Debug)]
pub struct SessionApprovals {
    always: Mutex<HashSet<ApprovalScope>>,
    /// The project the agreements are kept for between sessions, when they are.
    kept: Option<PathBuf>,
    /// Whether writes go ahead without being asked about, for as long as it is set.
    ///
    /// Writes and nothing else. A shell command and a request leaving the machine are the
    /// two things a person most wants to see before they happen, and a mode meant to stop
    /// the interruptions of a long editing run must not quietly cover them as well.
    writes: AtomicBool,
}

impl SessionApprovals {
    pub fn new() -> Self {
        Self::default()
    }

    /// The agreements kept for the project at `root`, which later ones are added to.
    pub fn kept_for(root: &Path) -> Self {
        let always = read_kept()
            .remove(&root.display().to_string())
            .unwrap_or_default()
            .into_iter()
            .collect();
        Self {
            always: Mutex::new(always),
            kept: Some(root.to_path_buf()),
            ..Self::default()
        }
    }

    /// Write the agreements held into the store of the project they are kept for.
    fn keep(&self, held: &HashSet<ApprovalScope>) {
        let (Some(root), Some(path)) = (&self.kept, kept_path()) else {
            return;
        };
        let mut all = read_kept();
        let key = root.display().to_string();
        if held.is_empty() {
            all.remove(&key);
        } else {
            all.insert(key, held.iter().cloned().collect());
        }
        let written = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|_| {
                std::fs::write(
                    &path,
                    serde_json::to_string_pretty(&all).unwrap_or_default(),
                )
            });
        if let Err(e) = written {
            tracing::warn!(path = %path.display(), error = %e, "agreements not kept");
        }
    }

    pub async fn is_always(&self, scope: &ApprovalScope) -> bool {
        self.always.lock().await.iter().any(|g| scope.covered_by(g))
    }

    pub async fn mark_always(&self, scope: ApprovalScope) {
        let mut held = self.always.lock().await;
        held.insert(scope);
        self.keep(&held);
    }

    /// Whether agreements outlive the session.
    pub fn keeps(&self) -> bool {
        self.kept.is_some()
    }

    /// What has been agreed to for the rest of this session, in the words the prompt
    /// used when it was agreed to.
    pub async fn standing(&self, root: &std::path::Path) -> Vec<String> {
        let mut out: Vec<String> = self
            .always
            .lock()
            .await
            .iter()
            .map(|s| s.describe_under(root))
            .collect();
        out.sort();
        out
    }

    /// Take back every standing agreement, so the next call asks again.
    pub async fn forget_always(&self) -> usize {
        let mut held = self.always.lock().await;
        let had = held.len();
        held.clear();
        self.keep(&held);
        had
    }

    /// Let writes go ahead unasked, or stop doing so.
    pub fn accept_writes(&self, on: bool) {
        self.writes.store(on, Ordering::Relaxed);
    }

    /// Whether this scope is covered by that standing decision.
    pub fn writes_accepted(&self, scope: &ApprovalScope) -> bool {
        matches!(scope, ApprovalScope::Path { .. }) && self.writes.load(Ordering::Relaxed)
    }
}

/// Where the agreements kept for each project are written.
fn kept_path() -> Option<PathBuf> {
    crate::paths::data_dir().map(|d| d.join("agreements.json"))
}

fn read_kept() -> std::collections::BTreeMap<String, Vec<ApprovalScope>> {
    kept_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Convenience: ask the UI for approval and wait for a decision. Returns
/// `Deny` if the UI channel is closed (e.g. user quit the app).
pub async fn ask(sender: &ApprovalSender, req: ApprovalRequest) -> ApprovalDecision {
    let (tx, rx) = oneshot::channel();
    if sender
        .send(ApprovalPrompt { req, reply: tx })
        .await
        .is_err()
    {
        return ApprovalDecision::Deny(None);
    }
    rx.await.unwrap_or(ApprovalDecision::Deny(None))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(s: &str) -> ApprovalScope {
        ApprovalScope::Command(s.into())
    }

    /// The failure this type exists to prevent: agreeing to one harmless command must not
    /// hand over the shell for the rest of the session.
    #[tokio::test]
    async fn agreeing_to_one_command_does_not_cover_another() {
        let a = SessionApprovals::new();
        a.mark_always(cmd("ls")).await;
        assert!(a.is_always(&cmd("ls")).await);
        assert!(!a.is_always(&cmd("rm -rf /")).await);
    }

    /// A command that merely starts with an approved one is a different command. Keying
    /// on the program at the front would let this through.
    #[tokio::test]
    async fn a_prefix_is_not_the_command_that_was_approved() {
        let a = SessionApprovals::new();
        a.mark_always(cmd("ls")).await;
        assert!(!a.is_always(&cmd("ls; rm -rf ~")).await);
        assert!(!a.is_always(&cmd("ls /etc")).await);
    }

    /// Agreeing while looking at a file covers the directory it lives in - and stops
    /// there.
    #[tokio::test]
    async fn a_write_agreement_covers_its_directory_and_no_further_up() {
        let a = SessionApprovals::new();
        a.mark_always(write_scope("write_file", Path::new("/p/src/main.rs")))
            .await;

        assert!(
            a.is_always(&write_scope("write_file", Path::new("/p/src/other.rs")))
                .await
        );
        assert!(
            a.is_always(&write_scope("write_file", Path::new("/p/src/ui/a.rs")))
                .await
        );
        assert!(
            !a.is_always(&write_scope("write_file", Path::new("/p/Cargo.toml")))
                .await
        );
    }

    /// Agreeing to writes by one tool says nothing about another tool's writes.
    #[tokio::test]
    async fn a_write_agreement_is_per_tool() {
        let a = SessionApprovals::new();
        a.mark_always(write_scope("write_file", Path::new("/p/src/main.rs")))
            .await;
        assert!(
            !a.is_always(&write_scope("apply_patch", Path::new("/p/src/main.rs")))
                .await
        );
    }

    /// Reaching one host is not reaching every host.
    #[tokio::test]
    async fn a_host_agreement_covers_one_host() {
        let a = SessionApprovals::new();
        a.mark_always(ApprovalScope::Host("docs.rs".into())).await;
        assert!(a.is_always(&ApprovalScope::Host("docs.rs".into())).await);
        assert!(
            !a.is_always(&ApprovalScope::Host("169.254.169.254".into()))
                .await
        );
    }

    /// An agreement kept for a project is there in its next session, for that project
    /// only, until it is taken back.
    #[test]
    fn an_agreement_is_kept_for_its_project() {
        let data = tempfile::tempdir().unwrap();
        crate::paths::with_env(&[("XDG_DATA_HOME", data.path().to_str())], || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            runtime.block_on(async {
                let root = Path::new("/somewhere/project");
                SessionApprovals::kept_for(root)
                    .mark_always(cmd("cargo test"))
                    .await;
                let next = SessionApprovals::kept_for(root);
                assert!(next.is_always(&cmd("cargo test")).await);
                let elsewhere = SessionApprovals::kept_for(Path::new("/elsewhere"));
                assert!(!elsewhere.is_always(&cmd("cargo test")).await);
                assert_eq!(next.forget_always().await, 1);
                let after = SessionApprovals::kept_for(root);
                assert!(!after.is_always(&cmd("cargo test")).await);
                assert!(!SessionApprovals::new().keeps());
            });
        });
    }

    /// Nothing is agreed to before the user has agreed to anything.
    #[tokio::test]
    async fn nothing_is_covered_by_default() {
        let a = SessionApprovals::new();
        assert!(!a.is_always(&cmd("ls")).await);
        assert!(!a.is_always(&ApprovalScope::Tool("run_tests".into())).await);
    }
}
