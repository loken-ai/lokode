use std::collections::HashSet;
use std::path::{Path, PathBuf};

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
        match self {
            ApprovalScope::Command(cmd) => format!("this exact command ({cmd})"),
            ApprovalScope::Path { tool, prefix } => {
                format!("{tool} anywhere under {}", prefix.display())
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
                ApprovalScope::Path { tool: gt, prefix: gp },
                ApprovalScope::Path { tool: st, prefix: sp },
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    Once,
    Always,
    Deny,
}

/// What a tool should do once the question has been settled, one way or another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    Proceed,
    Denied,
    /// The turn was abandoned while the prompt was on screen. Not the same as `Denied`:
    /// the user refused nothing, they stopped waiting.
    Interrupted,
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
}

/// Convenience: ask the UI for approval and wait for a decision. Returns
/// `Deny` if the UI channel is closed (e.g. user quit the app).
pub async fn ask(sender: &ApprovalSender, req: ApprovalRequest) -> ApprovalDecision {
    let (tx, rx) = oneshot::channel();
    if sender.send(ApprovalPrompt { req, reply: tx }).await.is_err() {
        return ApprovalDecision::Deny;
    }
    rx.await.unwrap_or(ApprovalDecision::Deny)
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

        assert!(a.is_always(&write_scope("write_file", Path::new("/p/src/other.rs"))).await);
        assert!(a.is_always(&write_scope("write_file", Path::new("/p/src/ui/a.rs"))).await);
        assert!(!a.is_always(&write_scope("write_file", Path::new("/p/Cargo.toml"))).await);
    }

    /// Agreeing to writes by one tool says nothing about another tool's writes.
    #[tokio::test]
    async fn a_write_agreement_is_per_tool() {
        let a = SessionApprovals::new();
        a.mark_always(write_scope("write_file", Path::new("/p/src/main.rs")))
            .await;
        assert!(!a.is_always(&write_scope("apply_patch", Path::new("/p/src/main.rs"))).await);
    }

    /// Reaching one host is not reaching every host.
    #[tokio::test]
    async fn a_host_agreement_covers_one_host() {
        let a = SessionApprovals::new();
        a.mark_always(ApprovalScope::Host("docs.rs".into())).await;
        assert!(a.is_always(&ApprovalScope::Host("docs.rs".into())).await);
        assert!(!a.is_always(&ApprovalScope::Host("169.254.169.254".into())).await);
    }

    /// Nothing is agreed to before the user has agreed to anything.
    #[tokio::test]
    async fn nothing_is_covered_by_default() {
        let a = SessionApprovals::new();
        assert!(!a.is_always(&cmd("ls")).await);
        assert!(!a.is_always(&ApprovalScope::Tool("run_tests".into())).await);
    }
}
