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
use crate::lsp::LspPool;
use crate::sandbox::approval::{self, ApprovalReceiver, SessionApprovals};
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
    /// What reading the standing instructions, and settling on a model, had to say.
    pub instruction_notes: Vec<String>,
    pub protocol: ToolProtocol,
    /// What the sessions started later read, once something has settled it.
    pub protocol_cell: crate::agent::protocol::SharedProtocol,
    /// What came back from a resumed session, for putting it on screen.
    pub restored: Vec<crate::backend::wire::Message>,
    /// Whether the window the backend serves is still to be found out, which is done
    /// once there is a screen to say it on.
    pub find_window: bool,
    /// The change history, if one could be opened.
    /// What has been agreed to for this session, so the interface can set a standing
    /// decision as well as the prompt.
    pub session_approvals: Arc<SessionApprovals>,
    pub checkpoints: Option<Arc<Checkpoints>>,
    /// Why there is no history, when there is not one.
    pub checkpoint_error: Option<String>,
    /// The tool servers that were started, and anything worth saying about them.
    pub mcp: crate::mcp::Loaded,
    /// Hands the agent a line the user typed while a turn was running.
    pub steer: tokio::sync::mpsc::Sender<String>,
    /// Every tool this session can call, named and described, built-ins and the servers'.
    pub tools: Vec<String>,
    /// The task list the model keeps, for the interface to draw.
    pub tasks: Arc<crate::tools::plan::TaskList>,
    /// What is known about this project and what is still to be looked into, shared with
    /// the background worker so a session sees what it learns.
    pub digest: Arc<tokio::sync::Mutex<crate::agent::digest::Digest>>,
    /// Answers one question about the project while nobody is waiting. `None` when the
    /// configuration turned it off.
    pub discovery: Option<Arc<crate::agent::background::Discovery>>,
    /// Lines the tools are printing as they print them.
    pub live: tokio::sync::mpsc::Receiver<crate::tools::registry::LiveOutput>,
}

pub async fn boot(cfg: &AppConfig) -> Result<Booted> {
    let tasks = Arc::new(crate::tools::plan::TaskList::default());
    let ((mut registry, lsp), _) =
        build_registry_with(cfg.root.clone(), tasks.clone(), cfg.lsp.clone());
    // The project's servers start reading it now, while the session opens.
    lsp.warm();
    // Third-party tools go in after the built-ins, so a server cannot take a name the agent
    // already relies on.
    let mcp = crate::mcp::load(&cfg.mcp_servers, &mut registry, &cfg.root).await;
    // The skills, offered by name and loaded when the model needs one.
    let (skills, skill_warnings) = crate::tools::skill::load(&crate::tools::skill::dirs(&cfg.root));
    if let Some(tool) = crate::tools::skill::LoadSkill::new(skills) {
        registry.register(tool);
    }
    // What the agents a turn hands work to choose their tools from. The delegation itself
    // is registered later, so it is not among them and an agent cannot delegate again.
    let registry_for_delegation = Arc::new(registry.clone());
    let registry = Arc::new(registry);
    // Registered last, because it hands work to a session built like this one. The
    // delegation is added to the shared registry below, once the backend is known.
    let (approval_tx, approval_rx) = approval::channel();
    let shutdown = CancellationToken::new();

    let auto_used = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    // Small on purpose: what a screen cannot keep up with is dropped rather than queued,
    // because a line that arrives late is worth less than the build finishing on time.
    let (live_tx, live_rx) = tokio::sync::mpsc::channel(64);
    // A script decides by its policy alone, so agreements kept from a person's sessions
    // are not its to use.
    let session_approvals = Arc::new(if cfg.headless {
        SessionApprovals::new()
    } else {
        SessionApprovals::kept_for(&cfg.root)
    });
    let mut ctx = ToolCtx {
        policy: Arc::new(cfg.policy.clone()),
        auto_used: auto_used.clone(),
        honour_allow: !cfg.headless,
        // A script reads the result; a person watches the build. Only the second wants
        // it a line at a time.
        live: (!cfg.headless).then(|| live_tx.clone()),
        call: None,
        root: cfg.root.clone(),
        approval: approval_tx,
        session_approvals: session_approvals.clone(),
        max_output_chars: crate::agent::session::tool_output_chars(
            cfg.context_tokens,
            cfg.max_answer_tokens,
        ),
        // Replaced with the turn's own token at the start of each turn; this stands in
        // for the window before the first one arrives.
        cancel: shutdown.child_token(),
        http: crate::tools::registry::tool_http_client()?,
        allow_hosts: Arc::new(cfg.allow_hosts.clone()),
        known: Default::default(),
        tail: Default::default(),
        sandbox: cfg.sandbox.clone().map(Arc::new),
        planning: cfg.mode == crate::agent::agent_loop::Mode::Plan,
    };

    let earlier = open_session(cfg)?;
    let backend = provider_for(cfg)?;
    // Nothing has to name a model. What was asked for wins, then what the session was
    // last using, then what the configuration remembers; failing all that the backend is
    // asked, and a backend that serves exactly one leaves nothing to choose.
    let mut model_notes = Vec::new();
    let model = match cfg.model.clone().or_else(|| {
        earlier
            .as_ref()
            .map(|e| e.model.clone())
            .filter(|m| !m.is_empty())
    }) {
        Some(m) => m,
        None => match backend.models().await {
            Ok(models) if models.len() == 1 => {
                let only = models.into_iter().next().unwrap_or_default();
                model_notes.push(format!(
                    "no model was named; the backend serves only {only}"
                ));
                only
            }
            // An interface can ask; a script cannot, which is why the headless path
            // refuses an unset model rather than guessing at one of several.
            Ok(models) if !models.is_empty() => {
                model_notes.push(format!(
                    "no model is set: /model to choose one of {}",
                    models.join(", ")
                ));
                String::new()
            }
            Ok(_) => {
                model_notes.push(
                    "no model is set and the backend lists none: name one with /model".into(),
                );
                String::new()
            }
            Err(e) => {
                model_notes.push(format!(
                    "no model is set and the backend does not say what it serves ({e:#}); \
                     name one with /model"
                ));
                String::new()
            }
        },
    };

    // `auto` starts native and drops to the envelope the first time a backend refuses it,
    // which is a decision the agent makes once and then keeps.
    let protocol = match cfg.tool_protocol {
        ProtocolChoice::Envelope => ToolProtocol::Envelope,
        ProtocolChoice::Auto | ProtocolChoice::Native => ToolProtocol::Native,
    };
    let protocol_cell = crate::agent::protocol::SharedProtocol::new(protocol);
    // A window belongs to a model: the one named for this session takes what the
    // configuration says it needs, and the default only when it says nothing.
    let per_model = cfg.models.get(&model);
    let context_tokens = per_model
        .and_then(|m| m.context_tokens)
        .unwrap_or(cfg.context_tokens);
    // What the backend actually offers, when nobody has said. A guess is wrong in both
    // directions: too small wastes the window that is there, and too large sends prompts
    // the server cuts, which produces an answer that is fluent and wrong about what it
    // was asked.
    // Found out after the interface is on screen, not before it. Asking a cold server
    // what window it serves means holding the model ready first, which is the whole load
    // of it - a minute of a blank terminal, with nothing anywhere saying why, because
    // none of this runs until there is a screen to say it on.
    let find_window = !cfg.context_was_set && per_model.and_then(|m| m.context_tokens).is_none();
    let asked_answer_tokens = per_model
        .and_then(|m| m.max_answer_tokens)
        .unwrap_or(cfg.max_answer_tokens);
    // The reserve comes out of the window before anything else is budgeted, so a window
    // that cannot afford the configured answer gets a shorter one rather than a client
    // with nothing left to read with.
    let max_answer_tokens =
        crate::agent::session::answer_reserve(context_tokens, asked_answer_tokens);
    if max_answer_tokens != asked_answer_tokens {
        model_notes.push(format!(
            "the window is {context_tokens} tokens, so the answer is capped at \
             {max_answer_tokens} rather than {asked_answer_tokens}"
        ));
    }

    // A tool result is only ever large relative to the window it lands in, so the
    // ceiling follows the model's window rather than the default.
    ctx.max_output_chars =
        crate::agent::session::tool_output_chars(context_tokens, max_answer_tokens);

    // What earlier sessions worked out about this project, and the questions they did
    // not get to. Both outlive the process, which is the whole point: the finding out is
    // done once and used every time.
    let mut digest = crate::agent::digest::Digest::open(crate::paths::state_dir(), &cfg.root);
    if digest.is_empty() && digest.questions.is_empty() {
        for question in crate::agent::background::opening_questions() {
            digest.ask(&question);
        }
        digest.save();
    }

    // Bounded against this window, and what could not be sent is said rather than
    // silently dropped.
    let (instructions, notes) = crate::config::load_instructions(
        &cfg.root,
        crate::agent::session::instruction_chars(context_tokens, max_answer_tokens),
    );
    // A resumed session comes back in the mode it was in, with its task list.
    let mode = match &earlier {
        Some(e) if e.planning => crate::agent::agent_loop::Mode::Plan,
        Some(_) => crate::agent::agent_loop::Mode::Act,
        None => cfg.mode,
    };
    if let Some(e) = &earlier {
        tasks.set(e.tasks.clone()).await;
    }
    ctx.planning = mode == crate::agent::agent_loop::Mode::Plan;
    // The standing instructions a person wrote, then what the background work has found
    // out: the cheaper half of what a session used to spend its first minutes
    // rediscovering, in a paragraph rather than in a dozen searches.
    let learned = digest.preamble(crate::agent::digest::MAX_PREAMBLE_CHARS);
    let instructions = match (instructions, learned) {
        (Some(i), Some(l)) => Some(format!("{i}\n\nWhat earlier sessions found out here:\n{l}")),
        (Some(i), None) => Some(i),
        (None, Some(l)) => Some(format!("What earlier sessions found out here:\n{l}")),
        (None, None) => None,
    };
    // And the shape of the tree. A model that has been told nothing about the project asks
    // for a file to be pasted in, because asking is cheaper than guessing a path.
    let instructions = match crate::agent::layout::preamble(&cfg.root) {
        Some(map) => Some(match instructions {
            Some(i) => format!("{i}\n\n{map}"),
            None => map,
        }),
        None => instructions,
    };
    let system = Agent::build_system_prompt(&registry, protocol, mode, instructions.as_deref());
    let mut session = Session::new(system, context_tokens, max_answer_tokens);
    let restored = earlier
        .as_ref()
        .map(|e| e.messages.clone())
        .unwrap_or_default();

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
            if mode == crate::agent::agent_loop::Mode::Plan {
                transcript.append(Record::Mode { planning: true });
            }
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
        restored_messages = restored.len(),
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

    // Read before the registry is handed to the agent, which takes it.
    let tools: Vec<String> = {
        let mut rows: Vec<String> = registry
            .iter()
            .map(|(name, tool)| {
                // A tool from a server carries its server in its name, which is the only
                // place the difference is recorded.
                let from = match name.split_once("__") {
                    Some((server, _)) => format!(" (from {server})"),
                    None => String::new(),
                };
                format!("{name}{from}: {}", tool.description())
            })
            .collect();
        rows.sort();
        rows
    };
    // A turn of its own, with its own window, for work whose search would otherwise fill
    // this one.
    let mut registry = Arc::try_unwrap(registry).unwrap_or_else(|shared| (*shared).clone());
    let role_or_session = |role| {
        cfg.roles
            .get(role)
            .map(str::to_string)
            .unwrap_or_else(|| model.clone())
    };
    // What the person types while a turn runs, and what agents working in the background
    // answer: both reach the session at its next step.
    let (steer_tx, steer_rx) = tokio::sync::mpsc::channel::<String>(8);
    let mut backends = std::collections::HashMap::new();
    for (name, b) in &cfg.backends {
        match provider(b.kind, &b.url, &b.headers) {
            Ok(p) => {
                backends.insert(name.clone(), p);
            }
            Err(e) => model_notes.push(format!("the backend {name} cannot be used: {e:#}")),
        }
    }
    let delegation = Arc::new(crate::tools::delegate::Delegation {
        backend: backend.clone(),
        backends,
        registry: registry_for_delegation.clone(),
        agents: cfg.agents.clone(),
        search_model: role_or_session(crate::agent::roles::Role::Search),
        edit_model: role_or_session(crate::agent::roles::Role::Edit),
        roles: cfg.roles.clone(),
        models: cfg.models.clone(),
        context_tokens,
        max_answer_tokens,
        protocol: protocol_cell.clone(),
        grammar_enabled: cfg.grammar_enabled,
        max_steps: cfg.max_steps,
        slots: Arc::new(tokio::sync::Semaphore::new(cfg.parallel_delegates.max(1))),
        parallel: cfg.parallel_delegates.max(1),
        hooks: cfg.hooks.clone(),
        lsp: Some(lsp.clone()),
        answers: Some(steer_tx.clone()),
        depth: cfg.delegate_depth.max(1),
    });
    model_notes.extend(delegation.unknown());
    model_notes.extend(skill_warnings);
    registry.register(crate::tools::delegate::Delegate::new(delegation));
    let registry = Arc::new(registry);

    let digest = Arc::new(tokio::sync::Mutex::new(digest));
    // Its own client, whose requests say they should yield, and the tools that read: a
    // background turn that could not read the project could not find anything out about
    // it. None of them changes anything or asks the user anything.
    let discovery = cfg
        .discover
        .then(|| -> Result<Arc<crate::agent::background::Discovery>> {
            Ok(Arc::new(crate::agent::background::Discovery {
                backend: background_provider(cfg)?,
                registry: Arc::new(
                    registry_for_delegation.read_only(crate::tools::delegate::NEVER),
                ),
                ctx: ctx.clone(),
                model: cfg
                    .roles
                    .get(crate::agent::roles::Role::Search)
                    .map(str::to_string)
                    .unwrap_or_else(|| model.clone()),
                protocol: protocol_cell.clone(),
                context_tokens,
                max_answer_tokens,
                digest: digest.clone(),
                stalls: Default::default(),
            }))
        })
        .transpose()?;

    let agent = Agent {
        this_turn: Default::default(),
        repairs: crate::agent::repairs::Repairs::open(crate::paths::state_dir(), &cfg.root),
        background: background_provider(cfg).ok(),
        checked_this_step: Default::default(),
        nudged: false,
        window_in_use: context_tokens,
        barren: Default::default(),
        failures: Default::default(),
        from_guess: Default::default(),
        repeats: Default::default(),
        before_plan: None,
        // What this project has taught about each model, from the writes that did and did
        // not check out. Kept on this machine, beside the state, never in the repository.
        history: crate::agent::history::ModelHistory::open(crate::paths::state_dir(), &cfg.root),
        escalated_from: None,
        clean_steps: 0,
        warming: Default::default(),
        spend: Default::default(),
        steps: Vec::new(),
        lsp: Some(lsp.clone()),
        steer: steer_rx,
        backend,
        registry,
        session,
        cfg: AgentConfig {
            model: model.clone(),
            mode,
            // Only worth doing, and only harmless, against a server whose weights are
            // near: a hosted endpoint has nothing to warm and would bill the attempt.
            preload: cfg.preload && crate::config::backend_is_nearby(&cfg.backend_url),
            guess_with: guess_model(cfg, &model),
            roles: cfg.roles.clone(),
            hooks: cfg.hooks.clone(),
            remember_model_in: crate::config::user_config_path(),
            context_tokens: cfg.context_tokens,
            models: cfg.models.clone(),
            instructions,
            grammar_enabled: cfg.grammar_enabled,
            max_steps: cfg.max_steps,
            max_answer_tokens: cfg.max_answer_tokens,
            protocol,
            allow_fallback: cfg.tool_protocol == ProtocolChoice::Auto,
            shared_protocol: Some(protocol_cell.clone()),
            delegated: false,
        },
        ctx,
        checkpoints: checkpoints.clone(),
        tasks: Some(tasks.clone()),
        mcp: mcp.clients.clone(),
        turn: 0,
    };

    Ok(Booted {
        session_approvals,
        protocol_cell,
        agent,
        lsp,
        approval_rx,
        shutdown,
        recorder,
        session_id,
        model,
        instruction_notes: [notes, model_notes].concat(),
        protocol,
        restored,
        find_window,
        checkpoints,
        checkpoint_error,
        mcp,
        steer: steer_tx,
        tools,
        tasks,
        digest,
        discovery,
        live: live_rx,
    })
}

/// A second client whose requests say they should yield to the user's.
///
/// The background work shares a machine with the turn a person is waiting on, so it asks
/// to be scheduled behind it. A server that does not schedule by priority ignores an
/// unknown header, which is the right outcome: the work still happens, just without the
/// courtesy. Only the OpenAI-compatible client carries it, because that is the one whose
/// server is on this machine.
/// Which model guesses the next read, or `None` for a session that does not guess.
///
/// Only against a backend on this machine or this network: a hosted endpoint bills for a
/// guess the same as for an answer. Which model is the project's business - what it names
/// for searching, or the one already answering - and where that model runs is the
/// server's: the request goes out at a priority that yields, so a cluster is free to put
/// it on another node and a single machine is free to make it wait.
fn guess_model(cfg: &AppConfig, answering: &str) -> Option<String> {
    if !cfg.speculate || !crate::config::backend_is_nearby(&cfg.backend_url) {
        return None;
    }
    Some(
        cfg.roles
            .get(crate::agent::roles::Role::Search)
            .unwrap_or(answering)
            .to_string(),
    )
}

fn background_provider(cfg: &AppConfig) -> Result<Arc<dyn crate::backend::provider::Provider>> {
    if cfg.backend_kind == crate::config::BackendKind::Anthropic {
        return provider_for(cfg);
    }
    Ok(Arc::new(BackendClient::with_headers(
        cfg.backend_url.clone(),
        &cfg.headers,
    )))
}

/// Build the client for whichever API the backend speaks.
///
/// The credential is resolved the same way for both - environment, then a named variable,
/// then a private file - and only where it is put on the wire differs: `Authorization:
/// Bearer` for the OpenAI-compatible family, `x-api-key` for Anthropic. Converting a
/// working configuration between them is a header change, not a value change.
fn provider_for(cfg: &AppConfig) -> Result<Arc<dyn crate::backend::provider::Provider>> {
    provider(cfg.backend_kind, &cfg.backend_url, &cfg.headers)
}

/// The client for a backend at `url` that speaks `kind`, sending `headers`.
fn provider(
    kind: crate::config::BackendKind,
    url: &str,
    headers: &[(String, String)],
) -> Result<Arc<dyn crate::backend::provider::Provider>> {
    match kind {
        crate::config::BackendKind::Anthropic => {
            let key = headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                .map(|(_, v)| v.trim_start_matches("Bearer ").to_string())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "the Anthropic API needs a key: set LOKODE_API_KEY, or name a variable \
                         with `api-key-env` in the configuration"
                    )
                })?;
            Ok(Arc::new(crate::backend::anthropic::AnthropicProvider::new(
                url.to_string(),
                &key,
            )?))
        }
        // `auto` is settled against the URL before it reaches here.
        _ => Ok(Arc::new(BackendClient::with_headers(
            url.to_string(),
            headers,
        ))),
    }
}

/// The twelve tools, and the language-server pool two of them share.
pub fn build_registry(root: PathBuf) -> (Registry, Arc<LspPool>) {
    build_registry_with(
        root,
        Arc::new(crate::tools::plan::TaskList::default()),
        crate::lsp::Languages::builtin(),
    )
    .0
}

/// [`build_registry`], keeping the task list the interface has to draw.
pub fn build_registry_with(
    root: PathBuf,
    tasks: Arc<crate::tools::plan::TaskList>,
    languages: crate::lsp::Languages,
) -> ((Registry, Arc<LspPool>), Arc<crate::tools::plan::TaskList>) {
    use crate::tools::{
        bash::Bash,
        fs_read::{Glob, ListDir, ReadFile},
        fs_write::{ApplyPatch, EditFile, EditLines, WriteFile},
        lsp::{LspAt, LspDiagnostics, LspSymbols},
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
    reg.register(EditLines);
    reg.register(crate::tools::plan::UpdatePlan {
        tasks: tasks.clone(),
    });
    reg.register(crate::tools::plan::SubmitPlan {
        tasks: tasks.clone(),
    });
    reg.register(ApplyPatch);
    reg.register(Bash);
    reg.register(RunTests);
    reg.register(WebFetch);
    // The language-server tools only where a server can answer for the project: offered
    // otherwise, they cost the model a tool list and calls that fail.
    let pool = LspPool::with(root, languages);
    if !pool.available().is_empty() {
        reg.register(LspDiagnostics { pool: pool.clone() });
        for tool in LspAt::all(&pool) {
            reg.register(tool);
        }
        reg.register(LspSymbols { pool: pool.clone() });
    }
    ((reg, pool), tasks)
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
