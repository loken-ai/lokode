use std::collections::HashSet;

use tokio::sync::{mpsc, oneshot, Mutex};

/// What kind of approval is being requested.
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub tool: String,
    pub summary: String,
    /// Optional unified-diff preview (write/edit/patch tools).
    pub diff_preview: Option<String>,
}

/// Outcome of a single approval prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    Once,
    Always,
    Deny,
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

/// Tracks per-tool "always" approvals for the lifetime of one session.
#[derive(Default, Debug)]
pub struct SessionApprovals {
    always: Mutex<HashSet<String>>,
}

impl SessionApprovals {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn is_always(&self, tool: &str) -> bool {
        self.always.lock().await.contains(tool)
    }

    pub async fn mark_always(&self, tool: &str) {
        self.always.lock().await.insert(tool.to_string());
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
