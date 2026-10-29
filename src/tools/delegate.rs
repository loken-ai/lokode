//! Handing a self-contained piece of work to a session of its own.
//!
//! The window is the scarce resource on a local model. A search that reads six files to
//! answer one question spends the whole conversation's budget on text nobody needs
//! afterwards - and once compaction drops it, what is left is the middle of a hunt with
//! no beginning. Delegating gives that hunt its own window and brings back only its
//! answer.
//!
//! What comes back is a summary, never the sub-session's history: the point is that the
//! caller's window holds the answer and not the search.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::tools::registry::{Registry, Tool, ToolCtx, ToolOutput};

/// Everything a delegated turn needs to be built, kept from the session that owns it.
///
/// Held as a builder rather than as an `Agent`: a sub-session is made and dropped per
/// call, and sharing one would share its history, which is the opposite of the point.
#[derive(Clone)]
pub struct Delegation {
    pub backend: Arc<dyn crate::backend::provider::Provider>,
    /// The other backends an agent can run on, by their name in `[backends]`.
    pub backends: std::collections::HashMap<String, Arc<dyn crate::backend::provider::Provider>>,
    /// The session's tools, without the delegation. Each agent gets its share of them.
    pub registry: Arc<Registry>,
    pub agents: Vec<crate::agent::agents::AgentSpec>,
    /// The model of an agent that names none and changes nothing: the search role, or the
    /// session's model.
    pub search_model: String,
    /// The model of an agent that names none and may change things: the edit role, or the
    /// session's model.
    pub edit_model: String,
    pub roles: crate::agent::roles::Roles,
    /// What each named model needs, for the window of an agent's model.
    pub models: std::collections::BTreeMap<String, crate::config::ModelSection>,
    /// The window of a model the configuration says nothing about.
    pub context_tokens: usize,
    pub max_answer_tokens: usize,
    pub protocol: crate::agent::protocol::SharedProtocol,
    pub grammar_enabled: bool,
    /// The session's step limit, which no agent goes past.
    pub max_steps: usize,
    /// How many delegated turns run at once. The others wait for a place.
    pub slots: Arc<tokio::sync::Semaphore>,
    /// How many places there are.
    pub parallel: usize,
    /// The hooks an agent that may change things runs, as the session does.
    pub hooks: crate::config::HooksSection,
    /// The language servers that check what an agent writes.
    pub lsp: Option<Arc<crate::lsp::LspPool>>,
    /// Where the answer of an agent working in the background is handed to the session.
    pub answers: Option<tokio::sync::mpsc::Sender<String>>,
    /// How many levels of agents may be under this one, itself included. An agent that
    /// names `delegate` among its tools has it when there is a level under it.
    pub depth: usize,
}

/// The tools a delegated session never has: the caller's task list and plan, and the
/// delegation.
pub const NEVER: &[&str] = &["update_plan", "submit_plan", "delegate"];

/// The steps of an agent that changes nothing and names no limit.
const READING_STEPS: usize = 8;

impl Delegation {
    /// The tools `spec` may use.
    fn tools_of(&self, spec: &crate::agent::agents::AgentSpec) -> Registry {
        match &spec.tools {
            None => self.registry.read_only(NEVER),
            Some(names) => {
                let mut out = Registry::new();
                for name in names.iter().filter(|n| !NEVER.contains(&n.as_str())) {
                    if let Some(tool) = self.registry.get(name) {
                        out.add(tool);
                    }
                }
                if self.depth > 1 && names.iter().any(|n| n == "delegate") {
                    out.register(Delegate::new(Arc::new(self.under())));
                }
                out
            }
        }
    }

    /// The delegation an agent hands work on with: one level less, and places of its own
    /// so an agent waiting on its own agents never holds the place they wait for.
    fn under(&self) -> Delegation {
        Delegation {
            depth: self.depth - 1,
            slots: Arc::new(tokio::sync::Semaphore::new(self.parallel.max(1))),
            ..self.clone()
        }
    }

    /// What the agents name that this session does not have, said once at startup.
    pub fn unknown(&self) -> Vec<String> {
        let mut out = Vec::new();
        for spec in &self.agents {
            match (&spec.backend, &spec.model) {
                (Some(b), _) if !self.backends.contains_key(b) => out.push(format!(
                    "the agent {} names the backend {b}, which [backends] does not have",
                    spec.name
                )),
                (Some(b), None) => out.push(format!(
                    "the agent {} runs on the backend {b} and names no model for it",
                    spec.name
                )),
                _ => {}
            }
            for name in spec.tools.iter().flatten() {
                if name == "delegate" && self.depth > 1 {
                    continue;
                }
                if self.registry.get(name).is_none() || NEVER.contains(&name.as_str()) {
                    out.push(format!(
                        "the agent {} names a tool it cannot have: {name}",
                        spec.name
                    ));
                }
            }
        }
        out
    }
}

pub struct Delegate {
    pub what: Arc<Delegation>,
    /// What the model reads about it, with the agents it can hand work to.
    description: String,
}

impl Delegate {
    pub fn new(what: Arc<Delegation>) -> Self {
        let agents: Vec<String> = what
            .agents
            .iter()
            .map(|a| format!("{} ({})", a.name, a.description))
            .collect();
        let description = format!(
            "Hand one self-contained task to an agent, in a session with its own context \
             window, and get back only its answer. Use it for a search that would read many \
             files, or for work an agent is named for. agent is one of: {}. Without agent, {}. \
             With background, an agent that changes nothing works while the turn goes on, and \
             its answer comes later.",
            agents.join("; "),
            crate::agent::agents::DEFAULT
        );
        Self { what, description }
    }

    /// Whether any of its agents may change something.
    fn any_writes(&self) -> bool {
        self.what
            .agents
            .iter()
            .any(|a| self.what.tools_of(a).iter().any(|(_, t)| t.mutates()))
    }
}

#[derive(Deserialize)]
struct Args {
    #[serde(default)]
    agent: Option<String>,
    task: String,
    #[serde(default)]
    background: bool,
}

#[async_trait]
impl Tool for Delegate {
    fn name(&self) -> &str {
        "delegate"
    }

    /// The sub-session's own tools ask for what they need, so the call itself changes
    /// nothing and is not gated twice.
    fn mutates(&self) -> bool {
        false
    }

    /// What an agent wrote is recorded once the call ends, under the caller's turn.
    fn may_change(&self) -> bool {
        self.any_writes()
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn args_schema(&self) -> Value {
        let names: Vec<&str> = self.what.agents.iter().map(|a| a.name.as_str()).collect();
        json!({
            "type": "object",
            "properties": {
                "agent": {"type": "string", "enum": names},
                "task": {"type": "string"},
                "background": {"type": "boolean"}
            },
            "required": ["task"],
            "additionalProperties": false
        })
    }

    async fn run(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput> {
        let a: Args = serde_json::from_value(args)?;
        if a.task.trim().is_empty() {
            return Ok(ToolOutput::err("say what to delegate"));
        }
        let w = &self.what;
        let name = a.agent.as_deref().unwrap_or(crate::agent::agents::DEFAULT);
        let Some(spec) = w.agents.iter().find(|s| s.name == name) else {
            let names: Vec<&str> = w.agents.iter().map(|s| s.name.as_str()).collect();
            return Ok(ToolOutput::err(format!(
                "there is no agent {name}; there is {}",
                names.join(", ")
            )));
        };
        // Its own backend when it names one, which it then needs a model for. The way the
        // session's backend takes tools is not that backend's, so it starts native.
        let (backend, protocol, elsewhere) = match &spec.backend {
            None => (w.backend.clone(), w.protocol.get(), false),
            Some(b) => match (w.backends.get(b), &spec.model) {
                (Some(p), Some(_)) => (
                    p.clone(),
                    crate::agent::protocol::ToolProtocol::Native,
                    true,
                ),
                (None, _) => {
                    return Ok(ToolOutput::err(format!(
                        "the agent {name} names the backend {b}, which [backends] does not have"
                    )))
                }
                (Some(_), None) => {
                    return Ok(ToolOutput::err(format!(
                        "the agent {name} runs on the backend {b} and names no model for it"
                    )))
                }
            },
        };
        let registry = Arc::new(w.tools_of(spec));
        let writes = registry.iter().any(|(_, t)| t.mutates());
        if a.background && writes {
            return Ok(ToolOutput::err(format!(
                "only an agent that changes nothing works in the background, and {name} may \
                 change things: hand it the task without background"
            )));
        }
        // A place among those that run at once, given up if the call is stopped first.
        let slot = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => return Ok(ToolOutput::err("interrupted by user")),
            slot = w.slots.clone().acquire_owned() => slot?,
        };
        let model = spec.model.clone().unwrap_or_else(|| match writes {
            true => w.edit_model.clone(),
            false => w.search_model.clone(),
        });
        let window = w
            .models
            .get(&model)
            .and_then(|m| m.context_tokens)
            .unwrap_or(w.context_tokens);
        let max_steps = spec
            .max_steps
            .unwrap_or(if writes { w.max_steps } else { READING_STEPS })
            .min(w.max_steps);
        // An agent that may change things changes nothing while its caller plans.
        let mode = match writes && !ctx.planning {
            true => crate::agent::agent_loop::Mode::Act,
            false => crate::agent::agent_loop::Mode::Plan,
        };
        let system = crate::agent::agent_loop::Agent::build_system_prompt(
            &registry,
            protocol,
            crate::agent::agent_loop::Mode::Act,
            Some(&spec.prompt),
        );
        let sub = crate::agent::agent_loop::Agent {
            this_turn: Default::default(),
            repairs: Default::default(),
            background: None,
            checked_this_step: Default::default(),
            nudged: false,
            window_in_use: 0,
            barren: Default::default(),
            failures: Default::default(),
            from_guess: Default::default(),
            repeats: Default::default(),
            before_plan: None,
            history: Default::default(),
            escalated_from: None,
            clean_steps: 0,
            warming: Default::default(),
            spend: Default::default(),
            steps: Vec::new(),
            lsp: w.lsp.clone().filter(|_| writes),
            steer: tokio::sync::mpsc::channel(1).1,
            backend,
            registry,
            session: crate::agent::session::Session::new(system, window, w.max_answer_tokens),
            cfg: crate::agent::agent_loop::AgentConfig {
                model,
                // The sub-session is short and its model is already chosen; nothing after
                // it is known, so there is nothing to hold ready.
                preload: false,
                guess_with: None,
                roles: w.roles.clone(),
                mode,
                hooks: w.hooks.clone(),
                instructions: None,
                remember_model_in: None,
                context_tokens: window,
                models: Default::default(),
                grammar_enabled: w.grammar_enabled,
                max_steps,
                max_answer_tokens: w.max_answer_tokens,
                protocol,
                // On a backend of its own, a refusal of native tools drops this session to
                // the envelope, as the main session's probe would have.
                allow_fallback: elsewhere,
                shared_protocol: None,
                delegated: true,
            },
            // The caller's context, so its approvals, its cancellation and its root are
            // the sub-session's too: this is the same user's work, delegated. Its own count
            // of what a policy granted, so its turn does not restart the caller's.
            ctx: crate::tools::registry::ToolCtx {
                auto_used: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                ..ctx.clone()
            },
            checkpoints: None,
            tasks: None,
            mcp: Vec::new(),
            turn: 0,
        };

        let task = a.task.clone();
        if a.background {
            // A job of its own: the turn does not wait for it and stopping the turn does
            // not stop it. Its answer is handed to the session when it comes.
            let ctx = crate::tools::registry::ToolCtx {
                cancel: tokio_util::sync::CancellationToken::new(),
                ..ctx.clone()
            };
            let (answers, hooks, agent) = (w.answers.clone(), w.hooks.clone(), name.to_string());
            tokio::spawn(async move {
                let _slot = slot;
                let out = finish(sub, task.clone(), agent.clone(), ctx, hooks).await;
                if let Some(tx) = answers {
                    let _ = tx
                        .send(format!(
                            "The agent {agent}, started in the background on \"{task}\", answered:\n{}",
                            out.content
                        ))
                        .await;
                }
            });
            return Ok(ToolOutput::ok(format!(
                "The agent {name} works on it in the background. Its answer is handed to you \
                 when it is ready: go on with something else meanwhile."
            )));
        }
        Ok(finish(sub, task, name.to_string(), ctx.clone(), w.hooks.clone()).await)
    }
}

/// Run a delegated session on `task` and bring back its answer, with the files it changed,
/// what the agent-end hooks said, and a word when it did not finish.
async fn finish(
    mut sub: crate::agent::agent_loop::Agent,
    task: String,
    name: String,
    ctx: ToolCtx,
    hooks: crate::config::HooksSection,
) -> ToolOutput {
    // Each call the sub-session makes is shown as it is made, under this call's step,
    // so delegated work running side by side is seen as lanes rather than as silence.
    let cancel = ctx.cancel.clone();
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let shown = ctx.clone();
    let drain = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            use crate::agent::agent_loop::AgentEvent;
            use crate::tools::registry::SubEvent;
            let sub = match &event {
                AgentEvent::ToolCall {
                    id,
                    name,
                    args_pretty,
                } => Some(SubEvent::Called {
                    id: id.clone(),
                    name: name.clone(),
                    args: args_pretty.clone(),
                }),
                AgentEvent::ToolResult {
                    id,
                    output,
                    took,
                    origin,
                    ..
                } => Some(SubEvent::Settled {
                    id: id.clone(),
                    is_error: output.is_error,
                    body: output.content.clone(),
                    took: *took,
                    origin: *origin,
                }),
                AgentEvent::AssistantText(s) if !s.trim().is_empty() => {
                    Some(SubEvent::Answer(s.clone()))
                }
                _ => None,
            };
            if let Some(sub) = sub {
                shown.report("delegate", sub).await;
            }
            events.push(event);
        }
        events
    });
    let outcome = sub.run_user_turn(task.clone(), cancel, tx).await;
    let events = drain.await.unwrap_or_default();
    if let Err(e) = outcome {
        return ToolOutput::err(format!("the delegated turn failed: {e:#}"));
    }
    // The answer is what it said last with nothing called after it. What it said
    // before a call is narration, and is returned as such when nothing came after.
    let mut answer = String::new();
    let mut called_after = false;
    let mut notices = Vec::new();
    for event in events {
        match event {
            crate::agent::agent_loop::AgentEvent::AssistantText(s) => {
                if !s.trim().is_empty() {
                    answer = s;
                    called_after = false;
                }
            }
            crate::agent::agent_loop::AgentEvent::ToolCall { .. } => called_after = true,
            crate::agent::agent_loop::AgentEvent::Notice(s) => notices.push(s),
            _ => {}
        }
    }
    if answer.trim().is_empty() {
        return ToolOutput::err(format!(
            "the delegated turn answered nothing{}",
            if notices.is_empty() {
                String::new()
            } else {
                format!(": {}", notices.join("; "))
            }
        ));
    }
    tracing::info!(
        steps = sub.spend.steps,
        answer_chars = answer.len(),
        "delegated turn finished"
    );
    // What it changed, in the words a write reports, so the caller counts the files
    // as its own turn's.
    if !sub.this_turn.files.is_empty() {
        let changed: Vec<String> = sub
            .this_turn
            .files
            .iter()
            .map(|f| format!("edited {f}"))
            .collect();
        answer = format!("{answer}\n\nIt changed:\n{}", changed.join("\n"));
    }
    // What the agent-end hooks make of the answer goes with it.
    let rans = crate::agent::hooks::run_all(
        &hooks.agent_end,
        crate::agent::hooks::Event::AgentEnd,
        &ctx.root,
        None,
        json!({
            "event": "agent-end",
            "root": ctx.root,
            "agent": name,
            "task": task,
            "answer": answer,
        }),
    )
    .await;
    if let Some(text) = crate::agent::hooks::said(crate::agent::hooks::Event::AgentEnd, &rans) {
        answer = format!("{answer}\n\n{text}");
    }
    if called_after {
        return ToolOutput::ok(ctx.clamp(format!(
            "It did not finish{}. What it said last, which may be partial:\n{answer}",
            if notices.is_empty() {
                String::new()
            } else {
                format!(" ({})", notices.join("; "))
            }
        )));
    }
    ToolOutput::ok(ctx.clamp(answer))
}
