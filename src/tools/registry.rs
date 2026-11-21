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
    self, Approval, ApprovalDecision, ApprovalRequest, ApprovalSender, Denial, SessionApprovals,
};

/// An editor holding the files the person has open, saved or not.
#[async_trait]
pub trait Editor: Send + Sync {
    /// The text the editor holds for `path`, or `None` when it does not say.
    async fn read(&self, path: &Path) -> Option<String>;
    /// Hand the editor `contents` for `path`.
    /// Says whether it took them.
    async fn write(&self, path: &Path, contents: &str) -> bool;
}

#[derive(Clone)]
pub struct ToolCtx {
    /// The editor this session runs in, which files are read from and written through.
    pub editor: Option<Arc<dyn Editor>>,
    /// What the configuration settles in advance, and how much of it one turn may use.
    pub policy: Arc<crate::sandbox::policy::Policy>,
    /// Requests the policy has granted this turn.
    /// Reset when a turn starts.
    pub auto_used: Arc<AtomicUsize>,
    /// Whether the session calling is planning, so that work it hands on changes nothing.
    pub planning: bool,
    /// Where a tool sends what it is printing, while it prints it.
    ///
    /// `None` for a caller with no screen.
    pub live: Option<tokio::sync::mpsc::Sender<LiveOutput>>,
    /// The id of the call being run, so what it prints reaches the step it belongs to.
    /// `None` outside a call.
    pub call: Option<String>,
    /// Whether the configuration's allow list applies.
    ///
    /// False for a run driven by a script, where the policy stated on the command line applies.
    /// A refusal in the configuration applies either way: nothing may widen one.
    pub honour_allow: bool,
    pub root: PathBuf,
    pub approval: ApprovalSender,
    pub session_approvals: Arc<SessionApprovals>,
    /// Characters a single tool result may contribute to the conversation.
    ///
    /// Derived from the session's budget, so a result fits the window.
    pub max_output_chars: usize,
    /// Characters a whole file or a command's output gives one step before only its ends are
    /// shown.
    ///
    /// Derived from the session's budget and the steps a turn is expected to take, so a turn
    /// that reads a file or runs a command each step still fits the window for that many steps.
    pub step_share_chars: usize,
    /// Cancelled when the user abandons the turn this tool call belongs to.
    ///
    /// A tool that can wait, on a subprocess or on a socket, must race its work against this.
    pub cancel: CancellationToken,
    /// Shared HTTP client, so a connection pool survives between calls.
    pub http: reqwest::Client,
    /// Hosts the user has declared reachable despite resolving to a private address.
    ///
    /// The address guard refuses them otherwise.
    pub allow_hosts: Arc<HashSet<String>>,
    /// What confines the commands the agent runs, or `None` to leave them unconfined.
    pub sandbox: Option<Arc<crate::sandbox::confine::Sandbox>>,
    /// What each file held the last time a tool read or wrote it.
    ///
    /// A whole-file write is written against what the model believes the file says.
    /// A change made elsewhere in between makes that belief out of date.
    pub known: Arc<std::sync::Mutex<HashMap<PathBuf, u64>>>,
    /// The end of what the call has printed, which the screen reads when told it changed.
    pub tail: Arc<Tail>,
    /// The files agents working in the background are changing, shared by the session.
    pub claims: Arc<Claims>,
    /// The agent working in the background this context belongs to, by number and name.
    /// `None` for the session and the agents it waits for.
    pub holder: Option<(u64, String)>,
    /// The work agents did in their own copies, waiting for the session to bring it in.
    pub works: crate::tools::worktree::Pending,
    /// The hooks told when the session waits for the person.
    pub notify: Arc<Vec<crate::config::Hook>>,
    /// What the person asked this session, in order.
    /// These are the project's objectives when an agent's work is reconciled with it.
    pub asked: Arc<std::sync::Mutex<Vec<String>>>,
    /// Whether the model takes images, learned from the backend.
    pub sees_images: bool,
}

/// The files each agent working in the background has written, held until it answers so
/// nothing else writes them under it.
#[derive(Default)]
pub struct Claims {
    held: std::sync::Mutex<HashMap<PathBuf, (u64, String)>>,
}

impl Claims {
    /// Take `path` for `holder`, the agent `agent`, or say who holds it.
    /// `None` stands for the session, which takes nothing and may write what nobody holds.
    pub fn take(&self, path: &Path, holder: Option<(u64, &str)>) -> Result<(), String> {
        let Ok(mut held) = self.held.lock() else {
            return Ok(());
        };
        match (held.get(path), holder) {
            (Some((by, agent)), mine) if mine.map(|(id, _)| id) != Some(*by) => Err(format!(
                "the agent {agent}, working in the background, is changing {}: leave it to \
                 that agent, or wait for its answer",
                path.display()
            )),
            (_, Some((id, agent))) => {
                held.insert(path.to_path_buf(), (id, agent.to_string()));
                Ok(())
            }
            (_, None) => Ok(()),
        }
    }

    /// Whether `holder` may write `path`, without taking it.
    pub fn check(&self, path: &Path, holder: Option<u64>) -> Result<(), String> {
        let Ok(held) = self.held.lock() else {
            return Ok(());
        };
        match held.get(path) {
            Some((by, agent)) if holder != Some(*by) => Err(format!(
                "the agent {agent}, working in the background, is changing {}: leave it to \
                 that agent, or wait for its answer",
                path.display()
            )),
            _ => Ok(()),
        }
    }

    /// Give back everything `holder` held.
    pub fn release(&self, holder: u64) {
        if let Ok(mut held) = self.held.lock() {
            held.retain(|_, (by, _)| *by != holder);
        }
    }
}

/// How many of the last lines a call printed are kept for the screen.
pub const TAIL_LINES: usize = 400;

/// The last lines a call printed, shared with the screen.
///
/// The screen is told once that there is something new and reads all of it then.
/// A call that prints faster than the screen draws loses no line of its tail.
#[derive(Debug, Default)]
pub struct Tail {
    lines: std::sync::Mutex<std::collections::VecDeque<String>>,
    told: std::sync::atomic::AtomicBool,
}

impl Tail {
    /// Keep `line`, and say whether the screen has to be told.
    /// It has to be told when it has read everything there was.
    pub fn push(&self, line: String) -> bool {
        if let Ok(mut lines) = self.lines.lock() {
            lines.push_back(line);
            while lines.len() > TAIL_LINES {
                lines.pop_front();
            }
        }
        !self.told.swap(true, std::sync::atomic::Ordering::AcqRel)
    }

    /// Forget that the screen was told, when telling it failed, so the next line tells it.
    pub fn untell(&self) {
        self.told.store(false, std::sync::atomic::Ordering::Release);
    }

    /// Everything kept, one line after another.
    /// A line kept after this tells the screen again.
    pub fn read(&self) -> String {
        self.told.store(false, std::sync::atomic::Ordering::Release);
        match self.lines.lock() {
            Ok(lines) => lines
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join("\n"),
            Err(_) => String::new(),
        }
    }
}

/// A cheap content fingerprint, for telling a file that changed from one that did not.
pub fn fingerprint(bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

impl ToolCtx {
    /// A path as it is worth showing: relative to the project.
    ///
    /// The absolute path is what the tools work with and what the sandbox checks.
    pub fn short(&self, path: &Path) -> String {
        if let Ok(rel) = path.strip_prefix(&self.root) {
            return rel.display().to_string();
        }
        // A resolved path is canonical and the configured root need not be.
        // A project reached through a symbolic link matches neither until the root is
        // canonical too.
        // The root is made canonical only on a miss.
        if let Ok(base) = self.root.canonicalize() {
            if let Ok(rel) = path.strip_prefix(&base) {
                return rel.display().to_string();
            }
        }
        path.display().to_string()
    }

    /// Remember what `path` holds now, so a later write can tell whether it still does.
    /// The text the editor holds for `path`, when the session runs in one that says.
    pub async fn editor_text(&self, path: &Path) -> Option<String> {
        match &self.editor {
            Some(e) => e.read(path).await,
            None => None,
        }
    }

    /// The text of `path`: the editor's, or the disk's.
    pub async fn read_text(&self, path: &Path) -> Result<String, String> {
        match self.editor_text(path).await {
            Some(text) => Ok(text),
            None => crate::tools::text::read_text(path).await,
        }
    }

    /// The same, `None` for a file that is not there.
    pub async fn read_text_if_exists(&self, path: &Path) -> Result<Option<String>, String> {
        match self.editor_text(path).await {
            Some(text) => Ok(Some(text)),
            None => crate::tools::text::read_text_if_exists(path).await,
        }
    }

    /// Whether this context may write `path`, asked before the person is.
    /// A write that would be refused is not worth a question.
    pub fn may_write(&self, path: &Path) -> Result<(), String> {
        self.claims
            .check(path, self.holder.as_ref().map(|(id, _)| *id))
    }

    /// Hand `contents` of `path` to the editor, when there is one, and make sure the disk
    /// holds them.
    /// The shell, the tests and the change history read the disk.
    pub async fn write_text(&self, path: &Path, contents: &str) -> anyhow::Result<()> {
        self.claims
            .take(path, self.holder.as_ref().map(|(id, a)| (*id, a.as_str())))
            .map_err(anyhow::Error::msg)?;
        if let Some(editor) = &self.editor {
            if editor.write(path, contents).await
                && tokio::fs::read_to_string(path).await.ok().as_deref() == Some(contents)
            {
                return Ok(());
            }
        }
        crate::tools::fs_write::write_atomic(path, contents).await
    }

    pub fn note_content(&self, path: &Path, bytes: &[u8]) {
        if let Ok(mut known) = self.known.lock() {
            known.insert(path.to_path_buf(), fingerprint(bytes));
        }
    }

    /// Whether `path` has changed since a tool last saw it.
    ///
    /// False for a file no tool has read.
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
/// Redirects are turned off.
/// `web_fetch` follows them itself, so the address guard runs again on every hop.
pub fn tool_http_client() -> Result<reqwest::Client> {
    tool_http_builder().build().map_err(Into::into)
}

/// The same client, with `host` fixed to the addresses that were checked.
///
/// The connection goes to one of them, and the name is not resolved a second time.
pub fn pinned_http_client(host: &str, addrs: &[std::net::SocketAddr]) -> Result<reqwest::Client> {
    tool_http_builder()
        .resolve_to_addrs(host, addrs)
        .build()
        .map_err(Into::into)
}

fn tool_http_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("lokode/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
}

impl ToolCtx {
    /// Settle whether this action may go ahead.
    ///
    /// Skips the question if its scope was already agreed to.
    /// Puts it to the user otherwise.
    /// Records an `Always`.
    /// Notices if the turn is abandoned while the prompt is on screen.
    /// Agree to a host without asking, and note it for `/net save`.
    ///
    /// A rule of the configuration that refuses it still answers first.
    /// A rule that asks is still asked about.
    /// The agreement lasts for the session: the host is asked about again in a later one.
    pub async fn record(&self, req: ApprovalRequest) -> Approval {
        match self.policy.verdict(
            &req.scope,
            &self.root,
            self.auto_used.load(Ordering::Relaxed),
        ) {
            crate::sandbox::policy::Verdict::Refused(why) => {
                tracing::info!(tool = %req.tool, scope = ?req.scope, %why, "refused by policy");
                return Approval::Denied(Denial::Rule(why));
            }
            crate::sandbox::policy::Verdict::Insist(_) => return self.approve(req).await,
            _ => {}
        }
        if let crate::sandbox::approval::ApprovalScope::Host(host) = &req.scope {
            self.session_approvals.audited(host);
        }
        Approval::Proceed
    }

    pub async fn approve(&self, req: ApprovalRequest) -> Approval {
        // The configuration is read first, and its refusals answer on their own.
        // A rule written down so that something never happens is not reachable by an
        // agreement given at the modal.
        let mut insist = false;
        match self.policy.verdict(
            &req.scope,
            &self.root,
            self.auto_used.load(Ordering::Relaxed),
        ) {
            crate::sandbox::policy::Verdict::Refused(why) => {
                tracing::info!(tool = %req.tool, scope = ?req.scope, %why, "refused by policy");
                return Approval::Denied(Denial::Rule(why));
            }
            crate::sandbox::policy::Verdict::Allowed if self.honour_allow => {
                self.auto_used.fetch_add(1, Ordering::Relaxed);
                tracing::info!(tool = %req.tool, scope = ?req.scope, "allowed by policy");
                return Approval::Proceed;
            }
            _ if !self.honour_allow
                && matches!(
                    req.scope,
                    crate::sandbox::approval::ApprovalScope::Unconfined(_)
                ) =>
            {
                return Approval::Denied(Denial::Rule(
                    "a command runs outside the sandbox only when a person agrees, and this \
                     run has nobody to ask"
                        .into(),
                ));
            }
            crate::sandbox::policy::Verdict::Insist(why) if !self.honour_allow => {
                tracing::info!(tool = %req.tool, scope = ?req.scope, %why, "nobody to ask");
                return Approval::Denied(Denial::Rule(format!(
                    "{why}, and this run has nobody to ask"
                )));
            }
            crate::sandbox::policy::Verdict::Insist(_) => insist = true,
            crate::sandbox::policy::Verdict::Allowed => {}
            crate::sandbox::policy::Verdict::Ask => {}
        }
        // A standing decision for writes, taken for a stretch of work.
        // It covers writes and nothing else.
        // A command and a request leaving the machine stay asked about.
        if !insist && self.session_approvals.writes_accepted(&req.scope) {
            tracing::info!(tool = %req.tool, scope = ?req.scope, "covered by accepting writes");
            return Approval::Proceed;
        }
        if !insist && self.session_approvals.is_always(&req.scope).await {
            tracing::info!(tool = %req.tool, scope = ?req.scope, "covered by an earlier agreement");
            return Approval::Proceed;
        }
        crate::sound::play(
            crate::sound::Event::Notification,
            crate::sound::Outcome::Success,
        );
        crate::agent::hooks::notify(
            &self.notify,
            &self.root,
            &format!("{} waits for you: {}", req.tool, req.summary),
        );
        let tool = req.tool.clone();
        let scope = req.scope.clone();
        let decision = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => {
                tracing::info!(%tool, "approval abandoned with the turn");
                return Approval::Interrupted;
            }
            d = approval::ask_for(&self.approval, req, self.call.clone()) => d,
        };
        // Recorded at every session: what the user agreed to, and how widely.
        tracing::info!(%tool, ?decision, ?scope, "approval decided");
        match decision {
            ApprovalDecision::Deny(why) => Approval::Denied(Denial::User(why)),
            ApprovalDecision::Once => Approval::Proceed,
            ApprovalDecision::Always => {
                self.session_approvals.mark_always(scope).await;
                Approval::Proceed
            }
        }
    }

    /// Report what a session this call runs is doing, a step at a time.
    /// Waited for, so a step that never reported its end is not shown running for ever.
    pub async fn report(&self, tool: &str, sub: SubEvent) {
        self.tell(tool, Live::Sub(sub)).await;
    }

    /// Hand the screen what stops this call alone, leaving the rest of the turn running.
    pub async fn offer_stop(&self, tool: &str) {
        self.tell(tool, Live::Stop(self.cancel.clone())).await;
    }

    /// Tell the screen about work this call leaves running: `doing` while it runs, `None`
    /// once it is over.
    pub async fn left_running(
        &self,
        tool: &str,
        id: u64,
        doing: Option<(String, CancellationToken)>,
    ) {
        self.tell(tool, Live::Aside { id, doing }).await;
    }

    async fn tell(&self, tool: &str, what: Live) {
        if let Some(live) = &self.live {
            let _ = live
                .send(LiveOutput {
                    tool: tool.to_string(),
                    call: self.call.clone(),
                    what,
                })
                .await;
        }
    }

    /// Keep a line a tool has just printed in the call's tail, and tell the screen when it
    /// has read the rest.
    /// Never waits.
    pub fn show(&self, tool: &str, line: &str) {
        if let Some(live) = &self.live {
            if self.tail.push(line.to_string()) {
                let told = live.try_send(LiveOutput {
                    tool: tool.to_string(),
                    call: self.call.clone(),
                    what: Live::Printed(self.tail.clone()),
                });
                if told.is_err() {
                    self.tail.untell();
                }
            }
        }
    }

    /// Cut `s` to what one result may contribute, saying so in the text so the model
    /// reads a deliberate elision rather than a file that ends mid-line.
    ///
    /// The notice states how much is left, so the model can ask for the rest.
    /// It asks by range where the tool takes one.
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

/// What a call tells the screen while it runs.
#[derive(Debug, Clone)]
pub struct LiveOutput {
    pub tool: String,
    /// The id of the call, when it comes from inside one.
    pub call: Option<String>,
    pub what: Live,
}

/// What a running call has for the screen.
#[derive(Debug, Clone)]
pub enum Live {
    /// Its tail changed.
    Printed(Arc<Tail>),
    /// A session the call runs did something.
    Sub(SubEvent),
    /// What stops this call alone.
    Stop(CancellationToken),
    /// Work the call left running after it answered, numbered `id`: what it is and what
    /// stops it, or `None` once it is over.
    Aside {
        id: u64,
        doing: Option<(String, CancellationToken)>,
    },
}

/// What a session run by a call did: a step asked for, a step settled, its answer.
#[derive(Debug, Clone)]
pub enum SubEvent {
    Called {
        id: String,
        name: String,
        args: String,
    },
    Settled {
        id: String,
        is_error: bool,
        body: String,
        took: Duration,
        origin: crate::agent::agent_loop::Origin,
    },
    Answer(String),
}

#[derive(Debug, Clone)]
pub struct ToolOutput {
    /// What the conversation carries to the model.
    pub content: String,
    /// What the person sees in place of `content`, when it differs.
    pub shown: Option<String>,
    pub is_error: bool,
    pub images: Vec<crate::backend::wire::Image>,
}

impl ToolOutput {
    pub fn ok(s: impl Into<String>) -> Self {
        Self {
            content: s.into(),
            shown: None,
            is_error: false,
            images: Vec::new(),
        }
    }
    pub fn err(s: impl Into<String>) -> Self {
        Self {
            content: s.into(),
            shown: None,
            is_error: true,
            images: Vec::new(),
        }
    }
    pub fn with_images(mut self, images: Vec<crate::backend::wire::Image>) -> Self {
        self.images = images;
        self
    }
    /// A shorter form for the conversation; a longer `shown` for the person.
    pub fn showing(mut self, shown: String) -> Self {
        self.shown = Some(shown);
        self
    }
    /// What the person sees: `shown` when set, else `content`.
    pub fn for_person(&self) -> &str {
        self.shown.as_deref().unwrap_or(&self.content)
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    /// Borrowed, because a tool can come from somewhere unknown at compile time.
    /// An MCP server names its own.
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema for the `arguments` object the model must emit.
    fn args_schema(&self) -> Value;

    /// Whether running this can leave the project different from how it found it.
    ///
    /// The loop takes a checkpoint around exactly the calls that need one.
    /// A tool that runs something is mutating even when it usually changes nothing.
    /// `bash` and the test runner both execute code that is free to write.
    fn mutates(&self) -> bool {
        false
    }

    /// Whether the tool is offered only while planning.
    fn planning_only(&self) -> bool {
        false
    }

    /// Whether a call can leave files changed without being gated as a change itself.
    /// An example is a call that hands work to a session whose own tools ask.
    /// Such a call is recorded for `/undo` once it ends.
    fn may_change(&self) -> bool {
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
    /// Returns false when the name is taken.
    /// A name is never overwritten, so no server shadows a built-in tool.
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
    /// The tools offered to the model, while planning or not.
    pub fn offered(&self, planning: bool) -> impl Iterator<Item = (&String, &Arc<dyn Tool>)> {
        self.iter()
            .filter(move |(_, tool)| planning || !tool.planning_only())
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

    /// Keep only the tools whose name `keep` holds to.
    pub fn retain(&mut self, keep: impl Fn(&str) -> bool) {
        self.tools.retain(|name, _| keep(name));
    }

    /// The tools that change nothing, without those named in `except`.
    pub fn read_only(&self, except: &[&str]) -> Registry {
        Registry {
            tools: self
                .tools
                .iter()
                .filter(|(name, tool)| !tool.mutates() && !except.contains(&name.as_str()))
                .map(|(name, tool)| (name.clone(), tool.clone()))
                .collect(),
        }
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

    /// A file an agent in the background wrote is its own until it is released.
    /// The session and the other agents are refused.
    /// The agent itself is not.
    #[test]
    fn a_file_is_held_by_the_agent_writing_it() {
        let claims = Claims::default();
        let path = Path::new("/p/notes.md");
        assert!(claims.take(path, None).is_ok());
        assert!(claims.take(path, Some((1, "scribe"))).is_ok());
        assert!(claims.take(path, Some((1, "scribe"))).is_ok());
        let refused = claims.take(path, None).unwrap_err();
        assert!(refused.contains("scribe"), "{refused}");
        assert!(claims.check(path, Some(2)).is_err());
        assert!(claims.check(path, Some(1)).is_ok());
        claims.release(1);
        assert!(claims.take(path, None).is_ok());
    }
    use crate::tools::harness::{Answer, Harness};

    /// The harness is returned, because the context it hands out points into a temporary
    /// directory it owns.
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

    /// Truncating by byte count lands mid-character on anything but ASCII.
    /// A cut inside a character panics.
    /// Emoji are 4 bytes, so a ceiling that is not a multiple of 4 falls inside one.
    #[tokio::test]
    async fn cutting_never_lands_inside_a_character() {
        for ceiling in 1..24 {
            let h = with_ceiling(ceiling);
            let out = h.ctx.clamp("🙂🙂🙂🙂🙂🙂".into());
            assert!(out.contains("truncated"), "ceiling {ceiling} did not cut");
            // Getting here means the cut was on a boundary.
            // No half-character was left behind.
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
