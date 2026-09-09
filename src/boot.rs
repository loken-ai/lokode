//! Assembling an agent from settings.
//!
//! The same work whether an interface or a script is going to drive it: build the registry,
//! resolve which session this is, decide how tools will be asked for, and wire the pieces
//! together. Written once here because the alternative - each entry point building its own -
//! is how the tool list came to be declared in two places and drift.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::agent::agent_loop::{Agent, AgentConfig};
use crate::agent::protocol::ToolProtocol;
use crate::agent::session::Session;
use crate::agent::transcript::{self, Record, Transcript};
use crate::app::{AppConfig, ProtocolChoice, Resume};
use crate::backend::client::BackendClient;
use crate::checkpoint::Checkpoints;
use crate::sandbox::approval::{self, ApprovalReceiver, SessionApprovals};
use crate::tools::lsp::LspPool;
use crate::tools::registry::{Registry, ToolCtx};

/// An agent and the handles somebody has to hold while it runs.
pub struct Booted {
    pub agent: Agent,
    pub lsp: Arc<LspPool>,
    /// Approval prompts the tools raise. Somebody has to answer these or the agent waits.
    pub approval_rx: ApprovalReceiver,
    /// Cancelled to bring everything down. Each turn's token is a child of it.
    pub shutdown: CancellationToken,
    /// The transcript writer, to be awaited before the process exits or the last records
    /// never reach the disk.
    pub recorder: Option<JoinHandle<()>>,
    pub session_id: String,
    pub model: String,
    /// What reading the standing instructions had to say, if anything.
    pub instruction_notes: Vec<String>,
    pub protocol: ToolProtocol,
    /// How many messages came back from a resumed session.
    pub restored: usize,
    /// The change history, if one could be opened.
    pub checkpoints: Option<Arc<Checkpoints>>,
    /// Why there is no history, when there is not one.
    pub checkpoint_error: Option<String>,
    /// The tool servers that were started, and anything worth saying about them.
    pub mcp: crate::mcp::Loaded,
}

pub async fn boot(cfg: &AppConfig) -> Result<Booted> {
    let (mut registry, lsp) = build_registry(cfg.root.clone());
    // Third-party tools go in after the built-ins, so a server cannot take a name the agent
    // already relies on.
    let mcp = crate::mcp::load(&cfg.mcp_servers, &mut registry, &cfg.root).await;
    let registry = Arc::new(registry);
    let (approval_tx, approval_rx) = approval::channel();
    let shutdown = CancellationToken::new();

    let ctx = ToolCtx {
        root: cfg.root.clone(),
        approval: approval_tx,
        session_approvals: Arc::new(SessionApprovals::new()),
        max_output_chars: crate::agent::session::tool_output_chars(
            cfg.context_tokens,
            cfg.max_answer_tokens,
        ),
        // Replaced with the turn's own token at the start of each turn; this stands in
        // for the window before the first one arrives.
        cancel: shutdown.child_token(),
        http: crate::tools::registry::tool_http_client()?,
        allow_hosts: Arc::new(cfg.allow_hosts.clone()),
        shell_writable: cfg.shell_writable.clone().map(Arc::new),
    };

    let earlier = open_session(cfg)?;
    let model = match cfg.model.clone().or_else(|| {
        earlier
            .as_ref()
            .map(|e| e.model.clone())
            .filter(|m| !m.is_empty())
    }) {
        Some(m) => m,
        None => anyhow::bail!("--model is required unless resuming a session that recorded one"),
    };

    // `auto` starts native and drops to the envelope the first time a backend refuses it,
    // which is a decision the agent makes once and then keeps.
    let protocol = match cfg.tool_protocol {
        ProtocolChoice::Envelope => ToolProtocol::Envelope,
        ProtocolChoice::Auto | ProtocolChoice::Native => ToolProtocol::Native,
    };
    // Bounded against this window, and what could not be sent is said rather than
    // silently dropped.
    let (instructions, notes) = crate::config::load_instructions(
        &cfg.root,
        crate::agent::session::instruction_chars(cfg.context_tokens, cfg.max_answer_tokens),
    );
    let system = Agent::build_system_prompt(&registry, protocol, instructions.as_deref());
    let mut session = Session::new(system, cfg.context_tokens, cfg.max_answer_tokens);
    let restored = earlier.as_ref().map(|e| e.messages.len()).unwrap_or(0);

    let mut recorder = None;
    let session_id = match earlier {
        Some(e) => {
            // Appending to the file it came from, rather than starting a new one: a
            // session that is resumed three times should be one story, not four.
            let (transcript, writer) = Transcript::create(session_path(&e.id)?)?;
            recorder = Some(writer);
            session = session.with_transcript(transcript, e.id.clone());
            session.restore(e.messages);
            e.id
        }
        None if cfg.transcripts => {
            let id = session.id.clone();
            let (transcript, writer) = Transcript::create(session_path(&id)?)?;
            recorder = Some(writer);
            transcript.append(Record::Meta {
                id: id.clone(),
                root: cfg.root.clone(),
                model: model.clone(),
                started_at: transcript::now_secs(),
            });
            session = session.with_transcript(transcript, id.clone());
            id
        }
        None => session.id.clone(),
    };

    tracing::info!(
        session = %session_id,
        model = %model,
        root = %cfg.root.display(),
        protocol = protocol.label(),
        restored_messages = restored,
        "session ready"
    );

    // A history of what the agent changes, so a turn can be shown or taken back. Not fatal
    // if it cannot be opened - git may be missing, or the root may be somewhere unusual -
    // but the user is told, because finding out later means finding out when it was needed.
    let (checkpoints, checkpoint_error) = match Checkpoints::open(&session_id, &cfg.root).await {
        Ok(cp) => (Some(Arc::new(cp)), None),
        Err(e) => {
            tracing::warn!(why = %format!("{e:#}"), "checkpoints unavailable");
            (None, Some(format!("{e:#}")))
        }
    };

    let agent = Agent {
        backend: provider_for(cfg)?,
        registry,
        session,
        cfg: AgentConfig {
            model: model.clone(),
            instructions,
            grammar_enabled: cfg.grammar_enabled,
            max_steps: cfg.max_steps,
            max_answer_tokens: cfg.max_answer_tokens,
            protocol,
            allow_fallback: cfg.tool_protocol == ProtocolChoice::Auto,
        },
        ctx,
        checkpoints: checkpoints.clone(),
        turn: 0,
    };

    Ok(Booted {
        agent,
        lsp,
        approval_rx,
        shutdown,
        recorder,
        session_id,
        model,
        instruction_notes: notes,
        protocol,
        restored,
        checkpoints,
        checkpoint_error,
        mcp,
    })
}

/// Build the client for whichever API the backend speaks.
///
/// The credential is resolved the same way for both - environment, then a named variable,
/// then a private file - and only where it is put on the wire differs: `Authorization:
/// Bearer` for the OpenAI-compatible family, `x-api-key` for Anthropic. Converting a
/// working configuration between them is a header change, not a value change.
fn provider_for(cfg: &AppConfig) -> Result<Arc<dyn crate::backend::provider::Provider>> {
    match cfg.backend_kind {
        crate::config::BackendKind::Anthropic => {
            let key = cfg
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                .map(|(_, v)| v.trim_start_matches("Bearer ").to_string())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "the Anthropic API needs a key: set VERVE_API_KEY, or name a variable \
                         with `api-key-env` in the configuration"
                    )
                })?;
            Ok(Arc::new(crate::backend::anthropic::AnthropicProvider::new(
                cfg.backend_url.clone(),
                &key,
            )?))
        }
        // `auto` is settled against the URL before it reaches here.
        _ => Ok(Arc::new(BackendClient::with_headers(
            cfg.backend_url.clone(),
            &cfg.headers,
        ))),
    }
}

/// The twelve tools, and the language-server pool two of them share.
pub fn build_registry(root: PathBuf) -> (Registry, Arc<LspPool>) {
    use crate::tools::{
        bash::Bash,
        fs_read::{Glob, ListDir, ReadFile},
        fs_write::{ApplyPatch, EditFile, WriteFile},
        lsp::{LspDiagnostics, LspHover},
        run_tests::RunTests,
        search::Grep,
        web_fetch::WebFetch,
    };

    let mut reg = Registry::new();
    reg.register(ReadFile);
    reg.register(ListDir);
    reg.register(Glob);
    reg.register(Grep);
    reg.register(WriteFile);
    reg.register(EditFile);
    reg.register(ApplyPatch);
    reg.register(Bash);
    reg.register(RunTests);
    reg.register(WebFetch);
    let pool = LspPool::new(root);
    reg.register(LspDiagnostics { pool: pool.clone() });
    reg.register(LspHover { pool: pool.clone() });
    (reg, pool)
}

/// Find the session being resumed, if one was asked for.
fn open_session(cfg: &AppConfig) -> Result<Option<transcript::Replay>> {
    let Some(which) = &cfg.resume else {
        return Ok(None);
    };
    let dir = crate::paths::sessions_dir()
        .ok_or_else(|| anyhow::anyhow!("no home directory, so no sessions to resume"))?;
    let path = match which {
        Resume::Id(id) => dir.join(format!("{id}.jsonl")),
        Resume::Latest => transcript::latest_for_root(&dir, &cfg.root)
            .ok_or_else(|| anyhow::anyhow!("no recorded session for {}", cfg.root.display()))?,
    };
    if !path.exists() {
        anyhow::bail!("no session at {}", path.display());
    }
    Ok(Some(transcript::replay(&path)?))
}

fn session_path(id: &str) -> Result<PathBuf> {
    Ok(crate::paths::sessions_dir()
        .ok_or_else(|| anyhow::anyhow!("no home directory to record a session in"))?
        .join(format!("{id}.jsonl")))
}
