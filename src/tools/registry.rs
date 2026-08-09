use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::sandbox::approval::{
    self, Approval, ApprovalDecision, ApprovalRequest, ApprovalSender, SessionApprovals,
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
    /// Shared HTTP client, so a connection pool survives between calls.
    pub http: reqwest::Client,
    /// Hosts the user has declared reachable despite resolving to a private address.
    ///
    /// Without this the address guard would make a locally-served doc site unreachable,
    /// which is a normal thing to want from a tool that runs on your own machine.
    pub allow_hosts: Arc<HashSet<String>>,
}

/// The HTTP client the tools share.
///
/// Redirects are turned off on purpose. `web_fetch` follows them itself so that the
/// address guard runs again on every hop; leaving the client to follow them would let an
/// approved public URL bounce to loopback with nobody looking.
pub fn tool_http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("verve/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

impl ToolCtx {
    /// Settle whether this action may go ahead.
    ///
    /// Everything a gated tool has to do about approval: skip the question if its scope
    /// was already agreed to, put it to the user otherwise, record an `Always`, and
    /// notice if the turn is abandoned while the prompt is on screen. Tools that wrote
    /// this out longhand each held their own copy of the rule about what `Always` means,
    /// which is how it came to mean the whole tool.
    pub async fn approve(&self, req: ApprovalRequest) -> Approval {
        if self.session_approvals.is_always(&req.scope).await {
            tracing::info!(tool = %req.tool, scope = ?req.scope, "covered by an earlier agreement");
            return Approval::Proceed;
        }
        let tool = req.tool.clone();
        let scope = req.scope.clone();
        let decision = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => {
                tracing::info!(%tool, "approval abandoned with the turn");
                return Approval::Interrupted;
            }
            d = approval::ask(&self.approval, req) => d,
        };
        // Worth recording at every session: what the user agreed to, and how widely, is
        // the first question anyone asks about a run that did something unexpected.
        tracing::info!(%tool, ?decision, ?scope, "approval decided");
        match decision {
            ApprovalDecision::Deny => Approval::Denied,
            ApprovalDecision::Once => Approval::Proceed,
            ApprovalDecision::Always => {
                self.session_approvals.mark_always(scope).await;
                Approval::Proceed
            }
        }
    }

    /// Cut `s` to what one result may contribute, saying so in the text so the model
    /// reads a deliberate elision rather than a file that ends mid-line.
    pub fn clamp(&self, mut s: String) -> String {
        if s.len() > self.max_output_chars {
            let cut = (0..=self.max_output_chars)
                .rev()
                .find(|&i| s.is_char_boundary(i))
                .unwrap_or(0);
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
        Self {
            content: s.into(),
            is_error: false,
        }
    }
    pub fn err(s: impl Into<String>) -> Self {
        Self {
            content: s.into(),
            is_error: true,
        }
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
        Self {
            tools: HashMap::new(),
        }
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
