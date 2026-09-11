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
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ApprovalScope {
    /// One shell command, matched in full.
    ///
    /// The whole string, not the program at the front of it: `ls` and `ls; rm -rf ~`
    /// share a first word, so keying on the program would let the second inherit an
    /// agreement given to the first. Matching the whole command is narrow enough to be
    /// honest and still spares the user the repeats an agent actually produces, which are
    /// the same command run again.
    Command(String),
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
    Denied(Option<String>),
    /// The turn was abandoned while the prompt was on screen. Not the same as `Denied`:
    /// the user refused nothing, they stopped waiting.
    Interrupted,
}

/// What a tool hands back when its call was refused.
///
/// Carries what the person said, so the model has something to act on rather than a
/// failure to work around.
pub fn denial(why: Option<String>) -> String {
    match why {
        None => "user denied".to_string(),
        Some(why) => format!("user denied: {why}"),
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

/// Tracks the `Always` agreements given during one session.
#[derive(Default, Debug)]
pub struct SessionApprovals {
    always: Mutex<HashSet<ApprovalScope>>,
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

    pub async fn is_always(&self, scope: &ApprovalScope) -> bool {
        self.always.lock().await.iter().any(|g| scope.covered_by(g))
    }

    pub async fn mark_always(&self, scope: ApprovalScope) {
        self.always.lock().await.insert(scope);
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

    /// Nothing is agreed to before the user has agreed to anything.
    #[tokio::test]
    async fn nothing_is_covered_by_default() {
        let a = SessionApprovals::new();
        assert!(!a.is_always(&cmd("ls")).await);
        assert!(!a.is_always(&ApprovalScope::Tool("run_tests".into())).await);
    }
}
