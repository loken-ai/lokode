use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::sandbox::approval::{
    self, ApprovalDecision, ApprovalRequest, ApprovalSender, SessionApprovals,
};

#[derive(Clone)]
pub struct ToolCtx {
    pub root: PathBuf,
    pub approval: ApprovalSender,
    pub session_approvals: Arc<SessionApprovals>,
    /// Characters a single tool result may contribute to the conversation.
    ///
    /// Derived from the session's budget rather than chosen per tool: a result is only
    /// ever large relative to the window it has to fit in, and a ceiling set without
    /// reference to that window lets one file read exceed the entire conversation.
    pub max_output_chars: usize,
    /// Cancelled when the user abandons the turn this tool call belongs to.
    ///
    /// A tool that can wait - on a subprocess, on a socket - must race its work against
    /// this, or `Esc` only takes effect once the thing the user wanted to stop has
    /// finished on its own.
    pub cancel: CancellationToken,
}

impl ToolCtx {
    /// Put a request to the user, unless the turn is abandoned while it is on screen.
    ///
    /// `None` means the turn was cancelled, which is not the same answer as `Deny`: the
    /// user refused nothing, they stopped waiting. Callers report it as an interruption
    /// so the model is not told it was denied something it never got asked about.
    pub async fn ask(&self, req: ApprovalRequest) -> Option<ApprovalDecision> {
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => None,
            d = approval::ask(&self.approval, req) => Some(d),
        }
    }

    /// Cut `s` to what one result may contribute, saying so in the text so the model
    /// reads a deliberate elision rather than a file that ends mid-line.
    pub fn clamp(&self, mut s: String) -> String {
        if s.len() > self.max_output_chars {
            let cut = (0..=self.max_output_chars).rev().find(|&i| s.is_char_boundary(i)).unwrap_or(0);
            s.truncate(cut);
            s.push_str("\n… (truncated to fit the context window)");
        }
        s
    }
}

#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn ok(s: impl Into<String>) -> Self {
        Self { content: s.into(), is_error: false }
    }
    pub fn err(s: impl Into<String>) -> Self {
        Self { content: s.into(), is_error: true }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    /// JSON Schema for the `arguments` object the model must emit.
    fn args_schema(&self) -> Value;

    async fn run(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput>;
}

pub struct Registry {
    tools: HashMap<&'static str, Arc<dyn Tool>>,
}

impl Registry {
    pub fn new() -> Self {
        Self { tools: HashMap::new() }
    }
    pub fn register<T: Tool + 'static>(&mut self, tool: T) {
        self.tools.insert(tool.name(), Arc::new(tool));
    }
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }
    pub fn iter(&self) -> impl Iterator<Item = (&&'static str, &Arc<dyn Tool>)> {
        self.tools.iter()
    }
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}
