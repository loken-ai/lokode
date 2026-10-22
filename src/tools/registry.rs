use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
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
    /// What the configuration settles in advance, and how much of it one turn may use.
    pub policy: Arc<crate::sandbox::policy::Policy>,
    /// Requests the policy has granted this turn. Reset when a turn starts, because a cap
    /// is about one piece of work rather than about a session.
    pub auto_used: Arc<AtomicUsize>,
    /// Where a tool sends what it is printing, while it prints it.
    ///
    /// A build takes minutes and says nothing until it is over, which is indistinguishable
    /// from a build that has hung. `None` for a caller with no screen: a script reads the
    /// result, and a line at a time would only be noise in its log.
    pub live: Option<tokio::sync::mpsc::Sender<LiveOutput>>,
    /// Whether the configuration's allow list applies.
    ///
    /// False for a run driven by a script, where the policy stated on the command line is
    /// the narrower statement and the one its author made for that run. A refusal in the
    /// configuration still applies either way: nothing may widen one.
    pub honour_allow: bool,
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
    /// Directories an approved shell command may write to, or `None` to leave it
    /// unconfined - which is what it has always been.
    pub shell_writable: Option<Arc<Vec<PathBuf>>>,
    /// What each file held the last time a tool read or wrote it.
    ///
    /// A whole-file write is written against what the model believes the file says. If
    /// someone changed it in their editor in between, that belief is out of date and the
    /// write silently discards their work.
    pub known: Arc<std::sync::Mutex<HashMap<PathBuf, u64>>>,
}

/// A cheap content fingerprint, for telling a file that changed from one that did not.
pub fn fingerprint(bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

impl ToolCtx {
    /// A path as it is worth showing: relative to the project, which is how everyone
    /// involved refers to it.
    ///
    /// The absolute path is what the tools work with and what the sandbox checks, but on
    /// a narrow screen it is a line of scaffolding in front of the three words that
    /// matter, and the model does not address files that way either.
    pub fn short(&self, path: &Path) -> String {
        if let Ok(rel) = path.strip_prefix(&self.root) {
            return rel.display().to_string();
        }
        // A resolved path is canonical and the configured root need not be, so a project
        // reached through a symbolic link matches neither until the root is canonical
        // too. Only on the miss: the common case costs nothing.
        if let Ok(base) = self.root.canonicalize() {
            if let Ok(rel) = path.strip_prefix(&base) {
                return rel.display().to_string();
            }
        }
        path.display().to_string()
    }

    /// Remember what `path` holds now, so a later write can tell whether it still does.
    pub fn note_content(&self, path: &Path, bytes: &[u8]) {
        if let Ok(mut known) = self.known.lock() {
            known.insert(path.to_path_buf(), fingerprint(bytes));
        }
    }

    /// Whether `path` has changed since a tool last saw it.
    ///
    /// False for a file no tool has read: nothing is known about it, and a guess either
    /// way would be worse than the caller's own judgement.
    pub fn changed_since_seen(&self, path: &Path, now: &[u8]) -> bool {
        let Ok(known) = self.known.lock() else {
            return false;
        };
        known
            .get(path)
            .is_some_and(|seen| *seen != fingerprint(now))
    }
}

/// The HTTP client the tools share.
///
/// Redirects are turned off on purpose. `web_fetch` follows them itself so that the
/// address guard runs again on every hop; leaving the client to follow them would let an
/// approved public URL bounce to loopback with nobody looking.
pub fn tool_http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("lokode/", env!("CARGO_PKG_VERSION")))
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
        // The configuration is read first, and its refusals answer on their own: a rule
        // written down so that something never happens must not be reachable by an
        // agreement given in a hurry at the modal.
        match self.policy.verdict(
            &req.scope,
            &self.root,
            self.auto_used.load(Ordering::Relaxed),
        ) {
            crate::sandbox::policy::Verdict::Refused(why) => {
                tracing::info!(tool = %req.tool, scope = ?req.scope, %why, "refused by policy");
                return Approval::Denied(None);
            }
            crate::sandbox::policy::Verdict::Allowed if self.honour_allow => {
                self.auto_used.fetch_add(1, Ordering::Relaxed);
                tracing::info!(tool = %req.tool, scope = ?req.scope, "allowed by policy");
                return Approval::Proceed;
            }
            crate::sandbox::policy::Verdict::Allowed => {}
            crate::sandbox::policy::Verdict::Ask => {}
        }
        // A standing decision for writes, taken deliberately for a stretch of work. It
        // covers writes and nothing else: a command and a request leaving the machine stay
        // asked about, because those are the two a person most wants to see coming.
        if self.session_approvals.writes_accepted(&req.scope) {
            tracing::info!(tool = %req.tool, scope = ?req.scope, "covered by accepting writes");
            return Approval::Proceed;
        }
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
            ApprovalDecision::Deny(why) => Approval::Denied(why),
            ApprovalDecision::Once => Approval::Proceed,
            ApprovalDecision::Always => {
                self.session_approvals.mark_always(scope).await;
                Approval::Proceed
            }
        }
    }

    /// Show a line a tool has just printed. Dropped rather than queued when the screen is
    /// behind: a stale line is worth nothing and holding up a build to deliver one would
    /// be worse than losing it.
    pub fn show(&self, tool: &str, line: &str) {
        if let Some(live) = &self.live {
            let _ = live.try_send(LiveOutput {
                tool: tool.to_string(),
                line: line.to_string(),
            });
        }
    }

    /// Cut `s` to what one result may contribute, saying so in the text so the model
    /// reads a deliberate elision rather than a file that ends mid-line.
    ///
    /// The notice states the arithmetic. A result that ends in a bare "truncated" reads
    /// like the end of the file, and a model that believes it has seen a whole file
    /// answers about the part it was not shown; one that is told how much is left asks
    /// for the rest, by range where the tool takes one.
    pub fn clamp(&self, mut s: String) -> String {
        if s.len() > self.max_output_chars {
            let whole = s.len();
            let cut = (0..=self.max_output_chars)
                .rev()
                .find(|&i| s.is_char_boundary(i))
                .unwrap_or(0);
            s.truncate(cut);
            s.push_str(&format!(
                "\n… (truncated to fit the context window: {cut} of {whole} characters \
                 shown, {} left unread; ask again for a later part)",
                whole - cut
            ));
        }
        s
    }
}

/// A line a tool has just printed, on its way to the screen.
#[derive(Debug, Clone)]
pub struct LiveOutput {
    pub tool: String,
    pub line: String,
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
    /// Borrowed rather than `&'static str`, because a tool can come from somewhere that
    /// was not known at compile time - an MCP server names its own.
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema for the `arguments` object the model must emit.
    fn args_schema(&self) -> Value;

    /// Whether running this can leave the project different from how it found it.
    ///
    /// Declared here rather than inferred from the tool's name, so that the loop can take a
    /// checkpoint around exactly the calls that need one. A tool that runs something is
    /// mutating even when it usually changes nothing: `bash` and the test runner both
    /// execute code that is free to write.
    fn mutates(&self) -> bool {
        false
    }

    async fn run(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput>;
}

#[derive(Clone)]
pub struct Registry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }
    pub fn register<T: Tool + 'static>(&mut self, tool: T) {
        self.add(Arc::new(tool));
    }

    /// Register something already behind a pointer, which is how a tool built at runtime
    /// arrives.
    ///
    /// Returns false when the name is taken. A silent overwrite would let one server
    /// shadow a built-in tool by naming itself after it.
    pub fn add(&mut self, tool: Arc<dyn Tool>) -> bool {
        let name = tool.name().to_string();
        if self.tools.contains_key(&name) {
            return false;
        }
        self.tools.insert(name, tool);
        true
    }
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Arc<dyn Tool>)> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::harness::{Answer, Harness};

    /// The harness is returned rather than dropped, because the context it hands out
    /// points into a temporary directory it owns.
    fn with_ceiling(n: usize) -> Harness {
        let mut h = Harness::new(Answer::No);
        h.ctx.max_output_chars = n;
        h
    }

    #[tokio::test]
    async fn output_under_the_ceiling_is_untouched() {
        let h = with_ceiling(100);
        assert_eq!(h.ctx.clamp("short".into()), "short");
    }

    #[tokio::test]
    async fn output_over_the_ceiling_says_it_was_cut() {
        let h = with_ceiling(10);
        let out = h.ctx.clamp("x".repeat(50));
        assert!(out.starts_with(&"x".repeat(10)));
        assert!(
            out.contains("truncated"),
            "the cut has to be visible: {out}"
        );
    }

    /// Truncating by byte count lands mid-character on anything but ASCII, and a `String`
    /// cut there is not a `String` at all - it panics. Emoji are four bytes, so a ceiling
    /// that is not a multiple of four falls inside one.
    #[tokio::test]
    async fn cutting_never_lands_inside_a_character() {
        for ceiling in 1..24 {
            let h = with_ceiling(ceiling);
            let out = h.ctx.clamp("🙂🙂🙂🙂🙂🙂".into());
            assert!(out.contains("truncated"), "ceiling {ceiling} did not cut");
            // Getting here at all means the cut was on a boundary; this pins down that
            // no half-character was left behind.
            assert!(
                !out.split("\n… (truncated")
                    .next()
                    .unwrap_or_default()
                    .contains('\u{fffd}'),
                "ceiling {ceiling} left a broken character"
            );
        }
    }

    #[tokio::test]
    async fn a_ceiling_below_one_character_yields_no_content() {
        let h = with_ceiling(1);
        let out = h.ctx.clamp("🙂".into());
        assert!(
            out.starts_with('\n') || out.starts_with('…'),
            "unexpected: {out:?}"
        );
    }

    #[tokio::test]
    async fn the_registry_answers_for_what_it_holds() {
        use crate::tools::fs_read::ListDir;
        let mut reg = Registry::new();
        assert!(reg.is_empty());
        reg.register(ListDir);
        assert_eq!(reg.len(), 1);
        assert!(reg.get("list_dir").is_some());
        assert!(reg.get("no_such_tool").is_none());
    }
}
