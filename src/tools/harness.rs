//! A `ToolCtx` to run tools against in tests.
//!
//! Every gated tool needs a root, an approval channel with something on the other end of
//! it, and a context to be handed. Written once here so that a tool's tests are about the
//! tool rather than about assembling one of these, and so the approval answers can be
//! chosen per case instead of stubbed the same way everywhere.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use crate::sandbox::approval::{self, ApprovalDecision, ApprovalRequest, SessionApprovals};
use crate::tools::registry::{Tool, ToolCtx, ToolOutput};

/// What the user does when asked.
#[derive(Clone, Copy, Debug)]
pub enum Answer {
    Yes,
    Always,
    No,
}

impl Answer {
    fn decision(self) -> ApprovalDecision {
        match self {
            Answer::Yes => ApprovalDecision::Once,
            Answer::Always => ApprovalDecision::Always,
            Answer::No => ApprovalDecision::Deny,
        }
    }
}

pub struct Harness {
    /// Held so the directory outlives the context pointing into it.
    _dir: TempDir,
    pub root: PathBuf,
    pub ctx: ToolCtx,
    asked: Arc<Mutex<Vec<ApprovalRequest>>>,
}

impl Harness {
    pub fn new(answer: Answer) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        // Canonicalised because the sandbox does the same to everything it is given, and
        // on macOS the temp directory is reached through a symlink.
        let root = dir.path().canonicalize().expect("canonicalize root");

        let (tx, mut rx) = approval::channel();
        let asked = Arc::new(Mutex::new(Vec::new()));
        let recorder = asked.clone();
        tokio::spawn(async move {
            while let Some(prompt) = rx.recv().await {
                recorder
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(prompt.req.clone());
                let _ = prompt.reply.send(answer.decision());
            }
        });

        let ctx = ToolCtx {
            policy: Arc::new(Default::default()),
            auto_used: Arc::new(Default::default()),
            honour_allow: true,
            live: None,
            root: root.clone(),
            approval: tx,
            session_approvals: Arc::new(SessionApprovals::new()),
            max_output_chars: 64 * 1024,
            cancel: CancellationToken::new(),
            http: crate::tools::registry::tool_http_client().expect("http client"),
            allow_hosts: Arc::new(Default::default()),
            shell_writable: None,
            known: Default::default(),
        };

        Self {
            _dir: dir,
            root,
            ctx,
            asked,
        }
    }

    /// A harness whose turn has already been abandoned.
    pub fn cancelled() -> Self {
        let h = Self::new(Answer::Yes);
        h.ctx.cancel.cancel();
        h
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    pub fn write(&self, rel: &str, body: &str) -> PathBuf {
        let p = self.path(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(&p, body).expect("write fixture");
        p
    }

    pub fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.path(rel)).expect("read back")
    }

    pub fn exists(&self, rel: &str) -> bool {
        self.path(rel).exists()
    }

    /// Every approval the tools have asked for, in order.
    pub fn asked(&self) -> Vec<ApprovalRequest> {
        self.asked.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Run a tool the way the agent loop does.
    ///
    /// A tool may report a failure either by returning `Ok` with an error output or by
    /// returning `Err`; `dispatch_tool` folds the second into the first so the model sees
    /// something either way. Tests go through the same fold, so they describe what the
    /// model would actually be told.
    pub async fn run(&self, tool: &dyn Tool, args: Value) -> ToolOutput {
        match tool.run(&self.ctx, args).await {
            Ok(o) => o,
            Err(e) => ToolOutput::err(format!("tool '{}' failed: {e:#}", tool.name())),
        }
    }
}

/// Assert a tool refused, and say what it actually did when it did not.
#[track_caller]
pub fn refused(out: &ToolOutput, containing: &str) {
    assert!(out.is_error, "expected a refusal, got: {}", out.content);
    assert!(
        out.content.contains(containing),
        "refusal did not mention {containing:?}: {}",
        out.content
    );
}

#[track_caller]
pub fn succeeded(out: &ToolOutput) {
    assert!(!out.is_error, "expected success, got: {}", out.content);
}

/// A path outside any project root, for the escape cases.
pub fn outside() -> &'static Path {
    Path::new("/etc/passwd")
}
