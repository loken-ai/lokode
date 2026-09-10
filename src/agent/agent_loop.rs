use std::sync::Arc;

use anyhow::Result;
use futures::StreamExt;
use jsonschema::Validator;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::protocol::{
    CallAccumulator, ModelTurn, ResolvedCall, ToolProtocol, TurnEnvelope,
};
use crate::agent::session::Session;
use crate::backend::client::{ChunkStream, StreamEvent, UNSUPPORTED_NATIVE_TOOLS};
use crate::backend::provider::{Provider, TurnRequest};
use crate::backend::repair::{self, MAX_REPAIR_ATTEMPTS};
use crate::backend::schema::{build_turn_schema, tool_manifest, tool_specs};
use crate::backend::wire::{FunctionCall, Message, ToolCall, Usage};
use crate::checkpoint::{Checkpoints, Snapshot};
use crate::tools::registry::{Registry, ToolCtx, ToolOutput};

/// What the loop is doing between one visible thing and the next.
///
/// A turn spends most of its time where nothing is printed: the request has gone out and
/// the model is reading a context that can be thousands of tokens, or a command is running
/// under a tool. Without a word for those the interface looks identical to one that has
/// stopped, which is what a user reports as "nothing is happening".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// The history is being fitted to the window before the request goes out.
    Preparing,
    /// The request is out and nothing has come back: the model is reading the prompt.
    Waiting,
    /// The answer is arriving.
    Answering,
    /// The model's scratchpad is arriving, before any of the answer.
    Thinking,
    /// A reply that did not parse is being repaired.
    Repairing,
    /// Tools are running, named in the order the model asked for them.
    Running(Vec<String>),
}

impl Phase {
    /// What the status line says of this phase.
    pub fn label(&self) -> String {
        match self {
            Phase::Preparing => "fitting the history".to_string(),
            Phase::Waiting => "waiting for the model".to_string(),
            Phase::Answering => "answering".to_string(),
            Phase::Thinking => "thinking".to_string(),
            Phase::Repairing => "repairing the reply".to_string(),
            Phase::Running(names) => match names.len() {
                1 => format!("running {}", names[0]),
                n => format!("running {} tools: {}", n, names.join(", ")),
            },
        }
    }
}

/// Whether the agent may change anything.
///
/// Planning is not a weaker set of approvals: it is a different question. A user who wants
/// to know what would happen should not have to say no to fifteen prompts to find out, and
/// an agent that has to ask before every step of a plan cannot describe the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Reads and searches; every tool that changes something is refused before it runs.
    Plan,
    #[default]
    Act,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Plan => "plan",
            Mode::Act => "act",
        }
    }
}

/// Something the interface asks of the agent between turns.
///
/// The agent runs in its own task and owns the session, so a question about the session -
/// which model, how full the window is - cannot be answered from the interface's side. It
/// is asked here and answered on the channel the asker holds.
pub enum Control {
    /// Switch model, or, with no name, say what could be switched to.
    Model {
        name: Option<String>,
        reply: tokio::sync::oneshot::Sender<String>,
    },
    /// Plan or act.
    Mode {
        mode: Mode,
        reply: tokio::sync::oneshot::Sender<String>,
    },
    /// What is holding the window.
    Context {
        reply: tokio::sync::oneshot::Sender<String>,
    },
    /// Compact now rather than when the window is full.
    Compact {
        reply: tokio::sync::oneshot::Sender<String>,
    },
    /// How full the window is, asked for the status line rather than for a person.
    Fullness {
        reply: tokio::sync::oneshot::Sender<u8>,
    },
    /// What the session has spent.
    Spend {
        reply: tokio::sync::oneshot::Sender<String>,
    },
    /// Why this model is the one answering.
    Why {
        reply: tokio::sync::oneshot::Sender<String>,
    },
}

/// Events the agent loop emits so the UI can render progress.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// Where the turn is, when nothing else is being printed.
    Phase(Phase),
    AssistantDelta(String),
    AssistantText(String),
    /// The model's scratchpad. Shown, never recorded, never sent back.
    Reasoning(String),
    ToolCall {
        name: String,
        args_pretty: String,
    },
    ToolResult {
        name: String,
        output: ToolOutput,
    },
    Notice(String),
    /// A line the user typed while the turn was running, now in the conversation.
    Steered(String),
    TurnDone,
}

/// What a write result says when the language server found something.
///
/// One string, because two places depend on it: the model reads it as part of the result,
/// and the step record reads it to know the check failed. A cascade would escalate on
/// exactly this.
const CHECK_FAILED: &str = "the language server reports:";

/// Left in the transcript when a turn ends without the model having answered.
///
/// The turn's user message is already in the history, and `Session::compact` drops whole
/// steps by looking for user-message boundaries. Leaving a user message with no reply
/// behind it puts two `user` roles in a row and moves a boundary the compactor reasons
/// about, so an abandoned turn is closed off rather than left open.
const TURN_ABANDONED: &str = "[this turn ended without an answer]";

/// How long to wait between two pieces of a streaming answer before giving up on it.
const STREAM_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Split a turn's calls into runs that may go at once.
///
/// A read-only call cannot see what another one did, so several of them are independent and
/// the order they finish in does not matter. A mutating call is a boundary on both sides:
/// what it changes is what a later call would read, and the checkpoint taken after it has to
/// describe that call alone.
///
/// A call to a tool that does not exist is its own run, because the answer is a refusal and
/// grouping it with anything would only obscure which call was refused.
fn runs(registry: &Registry, calls: Vec<ResolvedCall>) -> Vec<Vec<ResolvedCall>> {
    let mut out: Vec<Vec<ResolvedCall>> = Vec::new();
    for call in calls {
        let parallel = registry
            .get(&call.name)
            .map(|t| !t.mutates())
            .unwrap_or(false);
        match out.last_mut() {
            Some(last) if parallel && !last.is_empty() && last_is_parallel(registry, last) => {
                last.push(call)
            }
            _ => out.push(vec![call]),
        }
    }
    out
}

fn last_is_parallel(registry: &Registry, run: &[ResolvedCall]) -> bool {
    run.last()
        .and_then(|c| registry.get(&c.name))
        .map(|t| !t.mutates())
        .unwrap_or(false)
}

/// One model response, with whatever the server said about how it ended.
#[derive(Default)]
struct Completion {
    text: String,
    /// Native tool-call fragments, still to be reassembled.
    calls: CallAccumulator,
    /// `"length"` means the answer was cut off rather than finished.
    finish_reason: Option<String>,
    usage: Option<Usage>,
}

pub struct AgentConfig {
    pub model: String,
    pub mode: Mode,
    /// Whether to have the backend hold the next model ready while other work runs.
    /// Only ever true for a backend near enough for a load to be worth hiding.
    pub preload: bool,
    /// Which model does which kind of work, when the project says.
    pub roles: crate::agent::roles::Roles,
    /// Commands to run at the edges of a tool call and a turn.
    pub hooks: crate::config::HooksSection,
    /// The window to use for a model the configuration says nothing about.
    pub context_tokens: usize,
    /// What each named model needs, when it differs from the default.
    pub models: std::collections::BTreeMap<String, crate::config::ModelSection>,
    /// The standing instructions the preamble carries, kept so a protocol change or a
    /// model change can rebuild the preamble without reading the files again.
    pub instructions: Option<String>,
    /// Where a chosen model is written down for the next session. `None` remembers
    /// nothing, which is what a test and a one-shot run both want.
    pub remember_model_in: Option<std::path::PathBuf>,
    pub grammar_enabled: bool,
    pub max_steps: usize,
    pub max_answer_tokens: usize,
    /// How to ask for tools. Changed at most once per session, when a backend turns out
    /// not to support the native protocol.
    pub protocol: ToolProtocol,
    /// Whether a refusal of native tool calls may drop the session to the envelope.
    ///
    /// False when the user pinned a protocol: asking for one and silently getting the
    /// other is worse than being told the backend cannot do what was asked.
    pub allow_fallback: bool,
}

/// What a session has spent, so far.
///
/// The server already counts the tokens and nothing showed them: a person deciding
/// whether to hand the agent a large file has no way to know what the last one cost.
#[derive(Debug, Clone, Copy, Default)]
pub struct Spend {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub steps: u64,
    /// Time between the request going out and the answer being complete, summed.
    pub model_ms: u64,
    /// Time spent inside tools, summed.
    pub tool_ms: u64,
}

impl Spend {
    /// The line a status bar or a command can show.
    pub fn describe(&self) -> String {
        if self.steps == 0 {
            return "nothing spent yet this session".to_string();
        }
        let rate = if self.model_ms == 0 {
            0.0
        } else {
            self.completion_tokens as f64 * 1000.0 / self.model_ms as f64
        };
        format!(
            "{} steps: {} tokens in, {} out at {rate:.1}/s; {:.1}s in the model, {:.1}s in tools",
            self.steps,
            self.prompt_tokens,
            self.completion_tokens,
            self.model_ms as f64 / 1000.0,
            self.tool_ms as f64 / 1000.0
        )
    }
}

pub struct Agent {
    /// What each model has managed in this project, learned from the writes that did and
    /// did not check out.
    pub history: crate::agent::history::ModelHistory,
    /// The model this session was on before a failed check raised it, and how many clean
    /// steps have passed since. `None` when nothing has been raised.
    pub escalated_from: Option<String>,
    pub clean_steps: usize,
    /// Whether a model is being held ready right now, so a second request cannot queue
    /// behind the first.
    pub warming: Arc<std::sync::atomic::AtomicBool>,
    /// What this session has spent.
    pub spend: Spend,
    /// What each step asked for and what it cost. Recorded before anything routes on it,
    /// because a claim that one arrangement is faster needs the other one measured too.
    pub steps: Vec<crate::agent::steps::StepRecord>,
    /// The language servers, for checking a file the agent has just written. `None`
    /// leaves writes unchecked, which is what a test without a server wants.
    pub lsp: Option<Arc<crate::tools::lsp::LspPool>>,
    /// Lines the user typed while a turn was running. Drained at a step boundary, which
    /// is where the history can take a user message without moving a boundary the
    /// compactor reasons about.
    pub steer: mpsc::Receiver<String>,
    /// Whoever answers. The loop knows nothing about which API it is.
    pub backend: Arc<dyn Provider>,
    pub registry: Arc<Registry>,
    pub session: Session,
    pub cfg: AgentConfig,
    pub ctx: ToolCtx,
    /// Records what the agent changes, so a turn can be shown or taken back.
    pub checkpoints: Option<Arc<Checkpoints>>,
    /// Which turn this is, counted from one. Checkpoints are labelled with it, which is
    /// what lets `/undo` take back a turn rather than a single tool call.
    pub turn: u32,
}

impl Agent {
    /// The preamble, which states the contract the chosen protocol actually enforces.
    ///
    /// In the native protocol the tools travel in their own field and the server states
    /// the calling convention itself, so repeating a JSON schema here would describe a
    /// shape the model is not being asked for.
    pub fn build_system_prompt(
        reg: &Registry,
        protocol: ToolProtocol,
        mode: Mode,
        instructions: Option<&str>,
    ) -> String {
        let mut prompt = Self::contract(reg, protocol);
        // Stated in the contract rather than only enforced at dispatch: a model that
        // knows it is planning writes a plan, where one that is merely refused writes the
        // same call again.
        if mode == Mode::Plan {
            prompt.push_str(
                "\n\nYou are PLANNING. Read, search and ask, but do not change anything: \
                 every tool that writes, runs a command or reaches the network is refused \
                 while planning. Finish by stating the plan as numbered steps, each naming \
                 what you would run or change.",
            );
        }
        // Last, and named as the project's own: a model that has read the contract first
        // is less likely to take a project's wording for the calling convention.
        if let Some(text) = instructions.map(str::trim).filter(|t| !t.is_empty()) {
            prompt.push_str(
                "\n\nStanding instructions for this project and this user. Follow them \
                 unless the user says otherwise in the conversation.\n\n",
            );
            prompt.push_str(text);
            prompt.push('\n');
        }
        prompt
    }

    fn contract(reg: &Registry, protocol: ToolProtocol) -> String {
        match protocol {
            ToolProtocol::Native => format!(
                "You are a local-first code agent driving a TUI on the user's machine.\n\
                 Call the tools you have been given to inspect and change the project.\n\
                 Stop calling tools when the task is finished or when you only need to talk.\n\n\
                 {}",
                tool_manifest(reg)
            ),
            ToolProtocol::Envelope => format!(
                "You are a local-first code agent driving a TUI on the user's machine.\n\
                 Respond with a single JSON object matching the schema:\n\
                   {{ \"assistant_text\": \"<prose to user>\", \"tool_calls\": [ {{ \"name\": <tool>, \"arguments\": {{...}} }} ] }}\n\
                 Emit zero tool_calls when the task is finished or when you only need to talk.\n\
                 Tool results will be returned to you as role=tool messages on the next turn.\n\n\
                 {}",
                tool_manifest(reg)
            ),
        }
    }

    /// Swap protocol and rewrite the preamble to match.
    ///
    /// The system message states the contract, so changing how tools are asked for
    /// without changing it would leave the model reading instructions for the other one.
    fn fall_back_to_envelope(&mut self) {
        self.cfg.protocol = ToolProtocol::Envelope;
        self.rebuild_preamble();
    }

    /// Rewrite the contract the model is held to, from whatever it now is.
    fn rebuild_preamble(&mut self) {
        let preamble = Self::build_system_prompt(
            &self.registry,
            self.cfg.protocol,
            self.cfg.mode,
            self.cfg.instructions.as_deref(),
        );
        self.session.set_system_prompt(preamble);
    }

    /// What is holding the window, and how close it is to eliding.
    fn describe_context(&self) -> String {
        let size = self.session.size_chars();
        let budget = self.session.budget_chars();
        let mut lines = vec![format!(
            "{}% of the history budget, {size} characters of {budget}",
            self.session.fullness_percent()
        )];
        for (kind, chars) in self.session.occupancy() {
            let share = (chars * 100).checked_div(size).unwrap_or(0);
            lines.push(format!("  {share:>3}%  {kind}"));
        }
        lines.push(
            "past a hundred per cent the oldest steps are elided; /compact does it now".to_string(),
        );
        lines.join("\n")
    }

    /// Plan or act, from now on.
    fn set_mode(&mut self, mode: Mode) -> String {
        if self.cfg.mode == mode {
            return format!("already in {} mode", mode.label());
        }
        self.cfg.mode = mode;
        self.rebuild_preamble();
        // Planning is where reasoning is worth paying for and acting is where being over
        // quickly is, so the mode carries its model with it when the project named one.
        // Said out loud, because a model that changed without anyone announcing it would
        // make the next answer's character unexplainable.
        let role = match mode {
            Mode::Plan => crate::agent::roles::Role::Plan,
            Mode::Act => crate::agent::roles::Role::Chat,
        };
        // Planning is long and ends in acting, so the model acting will use is loaded
        // while the plan is being written. By the time /act is typed it is resident.
        if mode == Mode::Plan {
            let next = self
                .cfg
                .roles
                .get(crate::agent::roles::Role::Edit)
                .or_else(|| self.cfg.roles.get(crate::agent::roles::Role::Chat))
                .map(str::to_string);
            if let Some(next) = next {
                self.preload(&next);
            }
        }
        let switched = match self.cfg.roles.get(role) {
            Some(named) if named != self.cfg.model => {
                let to = named.to_string();
                let from = std::mem::replace(&mut self.cfg.model, to.clone());
                self.forget_model_calibration(&to);
                if let Some(t) = self.session.transcript() {
                    t.append(crate::agent::transcript::Record::Model { name: to.clone() });
                }
                tracing::info!(from = %from, to = %to, role = role.key(), "model followed the mode");
                format!(", on {to}")
            }
            _ => String::new(),
        };
        tracing::info!(mode = mode.label(), "mode changed");
        match mode {
            Mode::Plan => format!("planning{switched}: nothing will be changed until /act"),
            Mode::Act => {
                format!("acting{switched}: the agent may change things again, asking as it goes")
            }
        }
    }

    /// Have the backend hold `model` ready, behind whatever is happening now.
    ///
    /// A load is seconds of reading gigabytes onto a card. Asked for while a tool runs or
    /// while a plan is being written, those seconds are spent inside time that was going
    /// to pass anyway, and the switch that follows costs nothing.
    ///
    /// Fire and forget, and at most one at a time: this is an optimisation, so it must
    /// never delay a turn, fail one, or queue up behind itself. The server decides whether
    /// it can hold two models at once - that is its pressure protocol's job, not this
    /// one's - and if it cannot, the next use of the current model reloads it.
    fn preload(&self, model: &str) {
        if !self.cfg.preload || model == self.cfg.model || model.trim().is_empty() {
            return;
        }
        if self
            .warming
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        let backend = self.backend.clone();
        let warming = self.warming.clone();
        let model = model.to_string();
        tokio::spawn(async move {
            let started = std::time::Instant::now();
            match backend.warm(&model).await {
                Ok(()) => tracing::info!(
                    %model,
                    ms = started.elapsed().as_millis(),
                    "held ready for the next step"
                ),
                Err(e) => {
                    tracing::info!(%model, why = %format!("{e:#}"), "could not be held ready")
                }
            }
            warming.store(false, std::sync::atomic::Ordering::Relaxed);
        });
    }

    /// Take the window a model needs and forget what the last one taught, which every
    /// model change has to do whether a person asked for it or a role did.
    fn forget_model_calibration(&mut self, model: &str) {
        let per_model = self.cfg.models.get(model);
        let context = per_model
            .and_then(|m| m.context_tokens)
            .unwrap_or(self.cfg.context_tokens);
        let reserve = per_model
            .and_then(|m| m.max_answer_tokens)
            .unwrap_or(self.cfg.max_answer_tokens);
        self.session.set_window(context, reserve);
    }

    /// Answer one control request. Between turns, which is where the session can change
    /// under the model without a request being half-built against the old one.
    pub async fn control(&mut self, request: Control) {
        match request {
            Control::Model { name, reply } => {
                let answer = self.switch_model(name).await;
                let _ = reply.send(answer);
            }
            Control::Mode { mode, reply } => {
                let _ = reply.send(self.set_mode(mode));
            }
            Control::Context { reply } => {
                let _ = reply.send(self.describe_context());
            }
            Control::Compact { reply } => {
                let before = self.session.size_chars();
                let dropped = self.session.compact_now();
                let after = self.session.size_chars();
                let _ = reply.send(if dropped == 0 {
                    "nothing to elide: the window holds what is in it".to_string()
                } else {
                    format!(
                        "{dropped} message(s) elided; the window went from {}% to {}% full",
                        before * 100 / self.session.budget_chars().max(1),
                        after * 100 / self.session.budget_chars().max(1)
                    )
                });
            }
            Control::Fullness { reply } => {
                let _ = reply.send(self.session.fullness_percent());
            }
            Control::Why { reply } => {
                let _ = reply.send(self.explain_choice());
            }
            Control::Spend { reply } => {
                // The totals, then the last few steps: a session's cost is a sum, and
                // where it went is a list.
                let mut out = vec![self.spend.describe()];
                let recent: Vec<String> = self
                    .steps
                    .iter()
                    .rev()
                    .take(8)
                    .map(|s| format!("  {}", s.describe()))
                    .collect();
                if !recent.is_empty() {
                    out.push("last steps, most recent first:".to_string());
                    out.extend(recent);
                }
                let _ = reply.send(out.join("\n"));
            }
        }
    }

    /// Why this model is the one answering.
    fn explain_choice(&self) -> String {
        let mut lines = vec![format!("on {}", self.cfg.model)];
        if let Some(from) = &self.escalated_from {
            lines.push(format!(
                "raised from {from}, back down after two writes that check out"
            ));
        }
        if !self.cfg.roles.is_empty() {
            lines.push("by kind of work:".to_string());
            for (role, model) in self.cfg.roles.named() {
                lines.push(format!("  {role}: {model}"));
            }
        }
        lines.push(format!(
            "what this project has shown:\n{}",
            self.history.describe()
        ));
        lines.join("\n")
    }

    /// What the project says each kind of work should use.
    fn describe_roles(&self) -> String {
        if self.cfg.roles.is_empty() {
            return String::new();
        }
        let rows: Vec<String> = self
            .cfg
            .roles
            .named()
            .into_iter()
            .map(|(role, model)| format!("  {role}: {model}"))
            .collect();
        format!("\nby kind of work:\n{}", rows.join("\n"))
    }

    /// Switch to `name`, or describe what is on offer when there is none.
    ///
    /// A backend that cannot list its models is not a backend that cannot serve one, so a
    /// name is taken on trust when the listing fails. A name the backend does list and
    /// does not serve is refused, because the alternative is a session that fails on every
    /// turn with a message from the server.
    async fn switch_model(&mut self, name: Option<String>) -> String {
        let listed = self.backend.models().await;
        let Some(name) = name else {
            return match listed {
                Err(e) => format!(
                    "the backend does not say what it serves ({e:#}); name a model with /model <name>"
                ),
                Ok(models) if models.is_empty() => "the backend serves no model".to_string(),
                Ok(models) => {
                    let lines: Vec<String> = models
                        .iter()
                        .map(|m| {
                            let mark = if *m == self.cfg.model { "* " } else { "  " };
                            format!("{mark}{m}")
                        })
                        .collect();
                    format!(
                        "/model <name> to switch:\n{}{}",
                        lines.join("\n"),
                        self.describe_roles()
                    )
                }
            };
        };
        if let Ok(models) = &listed {
            if !models.is_empty() && !models.contains(&name) {
                return format!(
                    "the backend does not serve {name}; it serves:\n{}",
                    models.join("\n")
                );
            }
        }
        if name == self.cfg.model {
            return format!("already on {name}");
        }
        let previous = std::mem::replace(&mut self.cfg.model, name.clone());
        // A window belongs to a model, and so does the tokenizer the budget was
        // calibrated against; carrying either over is carrying a measurement of the
        // model that just left.
        self.forget_model_calibration(&name);
        let context = self.session.budget_chars();
        if let Some(t) = self.session.transcript() {
            t.append(crate::agent::transcript::Record::Model { name: name.clone() });
        }
        tracing::info!(from = %previous, to = %name, context, "model changed");
        // Remembered for the next session. Failing to write is worth saying and is not
        // worth refusing the switch over.
        let Some(path) = self.cfg.remember_model_in.clone() else {
            return format!("now on {name}, window {context}");
        };
        match crate::config::remember_model(&path, &name) {
            Ok(()) => format!(
                "now on {name}, window {context}; remembered in {}",
                path.display()
            ),
            Err(e) => format!("now on {name}, window {context}; could not remember it: {e:#}"),
        }
    }

    /// Run one user turn end-to-end. Loops model→tools→model until the
    /// model emits zero tool_calls, `max_steps` is reached, or `cancel` fires.
    ///
    /// `cancel` is created by the UI before the input is handed over, so the side that
    /// has to react to a keystroke already holds the handle it needs. It is stored on the
    /// tool context for the length of the turn, which is how a tool waiting on a
    /// subprocess learns that the user has stopped caring about the answer.
    /// The model for one kind of work: what the project named for that role, else the
    /// session's own model. Nothing here decides anything - it answers what is available
    /// to whoever is deciding.
    pub fn model_for(&self, role: crate::agent::roles::Role) -> String {
        self.cfg
            .roles
            .get(role)
            .map(str::to_string)
            .unwrap_or_else(|| self.cfg.model.clone())
    }

    /// Whether a turn can run at all.
    fn model_is_set(&self) -> bool {
        !self.cfg.model.trim().is_empty()
    }

    pub async fn run_user_turn(
        &mut self,
        user_input: String,
        cancel: CancellationToken,
        events: mpsc::Sender<AgentEvent>,
    ) -> Result<()> {
        // What this project taught, applied once, at the start. The ladder reacts to a
        // change that has already failed to compile, which costs a bad step every time;
        // the memory of those steps is what lets a turn start where the last one ended.
        if let Some(stronger) = self
            .cfg
            .roles
            .get(crate::agent::roles::Role::Escalate)
            .map(str::to_string)
        {
            if self.cfg.mode == Mode::Act
                && stronger != self.cfg.model
                && self.history.should_avoid(&self.cfg.model) == Some(true)
                && self.history.should_avoid(&stronger) != Some(true)
            {
                let from = std::mem::replace(&mut self.cfg.model, stronger.clone());
                self.forget_model_calibration(&stronger);
                self.escalated_from = Some(from.clone());
                tracing::info!(from = %from, to = %stronger, "started raised on what this project taught");
                let _ = events
                    .send(AgentEvent::Notice(format!(
                        "{from} has been failing its writes here, so this turn starts on {stronger}"
                    )))
                    .await;
            }
        }

        // The cap on what a policy may grant is about one piece of work, so it starts
        // again with each turn rather than running out over a session.
        self.ctx
            .auto_used
            .store(0, std::sync::atomic::Ordering::Relaxed);
        if !self.model_is_set() {
            let _ = events
                .send(AgentEvent::Notice(
                    "no model is set: /model lists what the backend serves".into(),
                ))
                .await;
            let _ = events.send(AgentEvent::TurnDone).await;
            return Ok(());
        }

        self.ctx.cancel = cancel.clone();
        self.turn += 1;
        tracing::info!(
            session = %self.session.id,
            chars = user_input.len(),
            "turn started"
        );
        self.session.push(Message::user(user_input));

        let turn_schema = build_turn_schema(&self.registry);
        let validator = Validator::new(&turn_schema).map_err(|e| anyhow::anyhow!("{e}"))?;

        for step in 0..self.cfg.max_steps {
            if cancel.is_cancelled() {
                self.abandon(&events, "interrupted").await;
                return Ok(());
            }
            // Before every call, not once per turn: a single tool result can be larger
            // than everything said so far, so the history can outgrow the window in the
            // middle of a turn as easily as between two.
            // Anything said while the previous step ran joins the conversation here,
            // before the request is built, so the model reads it on this step rather
            // than after the turn it was meant to change.
            while let Ok(said) = self.steer.try_recv() {
                self.session.push(Message::user(said.clone()));
                let _ = events.send(AgentEvent::Steered(said)).await;
            }
            let _ = events.send(AgentEvent::Phase(Phase::Preparing)).await;
            let dropped = self.session.compact();
            if dropped > 0 {
                tracing::warn!(
                    dropped,
                    budget_chars = self.session.budget_chars(),
                    size_chars = self.session.size_chars(),
                    "history compacted"
                );
                let _ = events
                    .send(AgentEvent::Notice(format!(
                        "context full: {dropped} earlier message(s) elided to keep the tool manifest in view"
                    )))
                    .await;
            }
            let sent_chars = self.session.size_chars();
            // Opened before the request goes out: everything here is what a decision
            // about which model to ask could have used, which is the point of recording
            // it while nothing is deciding anything yet.
            let mut record = crate::agent::steps::StepRecord {
                turn: self.turn,
                model: self.cfg.model.clone(),
                before: crate::agent::steps::Before {
                    context_chars: sent_chars,
                    context_percent: self.session.fullness_percent(),
                    step: step + 1,
                    previous_tools: self
                        .steps
                        .last()
                        .map(|s| s.after.tools.clone())
                        .unwrap_or_default(),
                    previous_failed_check: self
                        .steps
                        .last()
                        .map(|s| s.after.check_passed == Some(false))
                        .unwrap_or(false),
                    grammar: self.cfg.grammar_enabled,
                    mode: self.cfg.mode.label().to_string(),
                },
                after: Default::default(),
            };
            let _ = events.send(AgentEvent::Phase(Phase::Waiting)).await;
            let asked_at = std::time::Instant::now();
            let mut attempt = self.stream_one(&turn_schema, &cancel, &events).await;
            record.after.model_ms = asked_at.elapsed().as_millis() as u64;
            self.spend.model_ms += record.after.model_ms;

            // A backend that will not take native tool calls says so by refusing the
            // request. Nothing has been said to the user yet, so the same step can be run
            // again under the protocol that does not need the server's cooperation.
            if let Err(e) = &attempt {
                if self.cfg.allow_fallback
                    && self.cfg.protocol == ToolProtocol::Native
                    && format!("{e:#}").contains(UNSUPPORTED_NATIVE_TOOLS)
                {
                    tracing::warn!("falling back to the envelope protocol for this session");
                    self.fall_back_to_envelope();
                    let _ = events
                        .send(AgentEvent::Notice(
                            "backend does not do native tool calls; using the JSON envelope".into(),
                        ))
                        .await;
                    let _ = events.send(AgentEvent::Phase(Phase::Waiting)).await;
                    let asked_at = std::time::Instant::now();
                    attempt = self.stream_one(&turn_schema, &cancel, &events).await;
                    record.after.model_ms += asked_at.elapsed().as_millis() as u64;
                    self.spend.model_ms += asked_at.elapsed().as_millis() as u64;
                }
            }

            let completion = match attempt {
                Ok(c) => c,
                Err(_) if cancel.is_cancelled() => {
                    self.abandon(&events, "interrupted").await;
                    return Ok(());
                }
                Err(e) => {
                    tracing::error!(why = %format!("{e:#}"), "turn stopped on a backend error");
                    let _ = events
                        .send(AgentEvent::Notice(format!("backend error: {e:#}")))
                        .await;
                    self.abandon(&events, "stopped on a backend error").await;
                    return Ok(());
                }
            };

            // The server's count is the only tokenizer this client can reach, so it is
            // what the chars-per-token estimate is corrected against.
            self.spend.steps += 1;
            record.after.finish_reason = completion.finish_reason.clone();
            if let Some(usage) = completion.usage {
                record.after.prompt_tokens = usage.prompt_tokens;
                record.after.completion_tokens = usage.completion_tokens;
                self.spend.prompt_tokens += usage.prompt_tokens as u64;
                self.spend.completion_tokens += usage.completion_tokens as u64;
                if let Some(note) = self.session.observe(sent_chars, usage.prompt_tokens) {
                    let _ = events.send(AgentEvent::Notice(note)).await;
                }
            }
            // A response stopped by the token limit is a truncated JSON object. Without
            // this it goes into the repair path looking like a model that cannot write
            // JSON, and the repair fails for a reason nobody is told.
            tracing::info!(
                step = step + 1,
                sent_chars,
                answer_chars = completion.text.len(),
                finish_reason = completion.finish_reason.as_deref().unwrap_or("-"),
                prompt_tokens = completion.usage.map(|u| u.prompt_tokens),
                completion_tokens = completion.usage.map(|u| u.completion_tokens),
                "model responded"
            );
            if completion.finish_reason.as_deref() == Some("length") {
                tracing::warn!("answer truncated by the token limit");
                let _ = events
                    .send(AgentEvent::Notice(
                        "the answer hit the token limit and was cut off; \
                         raise --max-answer-tokens if this repeats"
                            .into(),
                    ))
                    .await;
            }

            let raw = completion.text.clone();
            let turn = match self
                .read_turn(completion, &turn_schema, &validator, &cancel, &events)
                .await
            {
                Ok(v) => v,
                Err(_) if cancel.is_cancelled() => {
                    self.abandon(&events, "interrupted").await;
                    return Ok(());
                }
                Err(e) => {
                    let _ = events
                        .send(AgentEvent::Notice(format!(
                            "step {}: gave up after {} repair attempts ({e}); raw output kept in transcript",
                            step + 1,
                            MAX_REPAIR_ATTEMPTS
                        )))
                        .await;
                    self.session.push(Message::assistant(raw));
                    break;
                }
            };

            if !turn.text.is_empty() {
                let _ = events
                    .send(AgentEvent::AssistantText(turn.text.clone()))
                    .await;
            }

            self.record_assistant_turn(&turn)?;

            if turn.calls.is_empty() {
                self.close_step(record);
                self.end_turn(&events).await;
                return Ok(());
            }
            record.after.tools = turn.calls.iter().map(|c| c.name.clone()).collect();

            // Read-only calls in a run go together. A turn that asks to read three files
            // waits for three round trips otherwise, one after another, for no reason -
            // none of them can see what the others did. A mutating call ends the run: what
            // it changes is what the next call would read, and a checkpoint has to sit
            // between them to mean anything.
            for run in runs(&self.registry, turn.calls) {
                if cancel.is_cancelled() {
                    // Every call the model just made needs a result, even the ones that
                    // will never run: the assistant turn listing them is already in the
                    // history, and a call with no answer behind it describes an action
                    // whose outcome the model cannot see.
                    for call in run {
                        self.session
                            .push(Message::tool("[error] interrupted by user", call.id));
                    }
                    continue;
                }

                // Announced before any of them starts, in the order the model asked, so
                // the screen reads as a list of what is happening rather than as whatever
                // finished first.
                for call in &run {
                    let args_pretty =
                        serde_json::to_string_pretty(&call.arguments).unwrap_or_default();
                    let _ = events
                        .send(AgentEvent::ToolCall {
                            name: call.name.clone(),
                            args_pretty,
                        })
                        .await;
                }

                let _ = events
                    .send(AgentEvent::Phase(Phase::Running(
                        run.iter().map(|c| c.name.clone()).collect(),
                    )))
                    .await;
                // The same trade inside a turn: a test suite or a build is seconds
                // during which the card is idle, and the model the next mode will want
                // can be read in behind it.
                if self.cfg.mode == Mode::Plan {
                    if let Some(next) = self
                        .cfg
                        .roles
                        .get(crate::agent::roles::Role::Edit)
                        .map(str::to_string)
                    {
                        self.preload(&next);
                    }
                }
                let ran_at = std::time::Instant::now();
                let results = futures::future::join_all(
                    run.iter()
                        .map(|call| self.dispatch_tool(&call.name, call.arguments.clone())),
                )
                .await;

                record.after.tool_ms += ran_at.elapsed().as_millis() as u64;
                self.spend.tool_ms += ran_at.elapsed().as_millis() as u64;
                for (call, result) in run.into_iter().zip(results) {
                    let _ = events
                        .send(AgentEvent::ToolResult {
                            name: call.name.clone(),
                            output: result.clone(),
                        })
                        .await;
                    let prefix = if result.is_error { "[error] " } else { "" };
                    self.session.push(Message::tool(
                        format!("{prefix}{}", result.content),
                        call.id,
                    ));

                    // After the call rather than before it: the state to return to is the
                    // one that preceded this turn, and that was recorded when the turn
                    // before it ended. Recording here also means a tool that changed
                    // nothing leaves nothing behind.
                    let mutating = self
                        .registry
                        .get(&call.name)
                        .map(|t| t.mutates())
                        .unwrap_or(false);
                    if mutating && !result.is_error {
                        record.after.wrote = true;
                        // The objective signal a cascade would escalate on. Recorded now,
                        // used by nothing yet.
                        record.after.check_passed = Some(!result.content.contains(CHECK_FAILED));
                        self.record_change(&call.name, &events).await;
                    }
                }
            }
            self.reconsider_model(&record, &events).await;
            self.close_step(record);
        }

        let _ = events
            .send(AgentEvent::Notice(format!(
                "stopped after {} steps (max_steps)",
                self.cfg.max_steps
            )))
            .await;
        self.end_turn(&events).await;
        Ok(())
    }

    /// Raise or lower the model from what the last step actually did.
    ///
    /// The signal is objective: the language server read the file the agent had just
    /// written and found something wrong with it. A router in a chat can only ask a model
    /// how confident it feels; a coding agent can compile. So the cheap path is tried
    /// first and the expensive one is paid for only where it has been shown to be needed.
    ///
    /// One rung, and down again after two clean steps. Two rungs would need evidence
    /// about which of them helps, and nothing here has that evidence yet.
    async fn reconsider_model(
        &mut self,
        last: &crate::agent::steps::StepRecord,
        events: &mpsc::Sender<AgentEvent>,
    ) {
        // Nothing writes while planning, so nothing there can fail a check.
        if self.cfg.mode == Mode::Plan {
            return;
        }
        let Some(stronger) = self.cfg.roles.get(crate::agent::roles::Role::Escalate) else {
            return;
        };
        let stronger = stronger.to_string();

        if last.after.check_passed == Some(false) && self.escalated_from.is_none() {
            if stronger == self.cfg.model {
                return;
            }
            let from = std::mem::replace(&mut self.cfg.model, stronger.clone());
            self.forget_model_calibration(&stronger);
            self.escalated_from = Some(from.clone());
            self.clean_steps = 0;
            if let Some(t) = self.session.transcript() {
                t.append(crate::agent::transcript::Record::Model {
                    name: stronger.clone(),
                });
            }
            tracing::info!(from = %from, to = %stronger, "raised after a failed check");
            let _ = events
                .send(AgentEvent::Notice(format!(
                    "the change did not check out; carrying on with {stronger}"
                )))
                .await;
            return;
        }

        // Down again once the work is going well, so the expensive model is paid for
        // where it was needed and not for the rest of the turn. Only a write that checked
        // out counts: a step that read a file says nothing about whether the cheaper
        // model can produce a change that compiles, and taking it as evidence would send
        // the session back down on the strength of two searches.
        if self.escalated_from.is_some() && last.after.check_passed == Some(true) {
            self.clean_steps += 1;
            if self.clean_steps < 2 {
                return;
            }
            let Some(back) = self.escalated_from.take() else {
                return;
            };
            self.clean_steps = 0;
            let from = std::mem::replace(&mut self.cfg.model, back.clone());
            self.forget_model_calibration(&back);
            if let Some(t) = self.session.transcript() {
                t.append(crate::agent::transcript::Record::Model { name: back.clone() });
            }
            tracing::info!(from = %from, to = %back, "lowered after two clean steps");
            let _ = events
                .send(AgentEvent::Notice(format!("back on {back}")))
                .await;
        }
    }

    /// Keep one step's measurement, and write it where a later session can read it.
    fn close_step(&mut self, record: crate::agent::steps::StepRecord) {
        // A write that did or did not check out is the only thing here worth remembering
        // past the session: it is the evidence the next one starts from.
        if let Some(passed) = record.after.check_passed {
            self.history.record(&record.model, passed);
        }
        tracing::info!(
            turn = record.turn,
            step = record.before.step,
            model = %record.model,
            tools = ?record.after.tools,
            model_ms = record.after.model_ms,
            tool_ms = record.after.tool_ms,
            check = ?record.after.check_passed,
            "step"
        );
        if let Some(t) = self.session.transcript() {
            t.append(crate::agent::transcript::Record::Step(record.clone()));
        }
        // Bounded: a long session's steps are on disk, and what is kept here is only what
        // a person or a later decision would look back over.
        self.steps.push(record);
        if self.steps.len() > 200 {
            self.steps.remove(0);
        }
    }

    /// Diagnostics for a file a write tool just changed, when there are any.
    ///
    /// The path is read from the tool's own result rather than from its arguments: what
    /// was written is what the tool says it wrote, and a refused or failed call says
    /// nothing to parse. Never fails a write: a language server that is missing, slow or
    /// silent leaves the result as it was.
    async fn verify_written(&self, tool: &str, out: &ToolOutput) -> Option<String> {
        let lsp = self.lsp.as_ref()?;
        if out.is_error || !matches!(tool, "write_file" | "edit_file" | "edit_lines") {
            return None;
        }
        // "edited <path>", "wrote <path>", "applied N edit(s) to <path>".
        let first = out.content.lines().next()?;
        let path = first.rsplit(' ').next()?;
        let path = std::path::Path::new(path);
        if !path.exists() {
            return None;
        }
        crate::tools::lsp::diagnostics_now(lsp, path, std::time::Duration::from_secs(2)).await
    }

    /// Run the turn-end hook, if there is one, and close the turn.
    ///
    /// What the hook says is a notice rather than part of the conversation: the turn is
    /// over, so there is nothing left for the model to do about it, and the person is the
    /// one who has to read that the formatter rewrote three files.
    async fn end_turn(&self, events: &mpsc::Sender<AgentEvent>) {
        // Written at the end of a turn rather than after every step: losing it costs a
        // memory, not a session, and a turn is the unit a person would call a piece of
        // work anyway.
        self.history.save();
        if let Some(hook) = self.cfg.hooks.turn_end.clone() {
            let ran = crate::agent::hooks::run(&hook, &self.ctx.root, "", "turn-end").await;
            if !ran.output.is_empty() {
                let mark = if ran.ok {
                    "turn-end hook"
                } else {
                    "turn-end hook failed"
                };
                let _ = events
                    .send(AgentEvent::Notice(format!("{mark}:\n{}", ran.output)))
                    .await;
            }
        }
        let _ = events.send(AgentEvent::TurnDone).await;
    }

    /// Close an unanswered turn.
    ///
    /// The user message is already in the history; without a reply behind it the next
    /// turn puts two `user` roles in a row, which is not a shape the compactor's
    /// boundary search expects.
    async fn abandon(&mut self, events: &mpsc::Sender<AgentEvent>, why: &str) {
        self.session.push(Message::assistant(TURN_ABANDONED));
        let _ = events.send(AgentEvent::Notice(why.to_string())).await;
        // The hook runs here too: an abandoned turn can have changed files, and that is
        // exactly when a formatter or a test suite is worth running.
        self.end_turn(events).await;
    }

    /// Commit what a tool just changed, if a history is being kept.
    ///
    /// A failure to record is reported and not fatal: losing the ability to undo is bad,
    /// and refusing to continue the turn over it would be worse.
    async fn record_change(&self, tool: &str, events: &mpsc::Sender<AgentEvent>) {
        let Some(cp) = &self.checkpoints else { return };
        match cp.snapshot(tool, self.turn).await {
            Ok(Snapshot::Recorded(sha)) => {
                tracing::info!(tool, turn = self.turn, %sha, "checkpoint recorded")
            }
            Ok(Snapshot::Unchanged) => {}
            Err(e) => {
                tracing::warn!(tool, why = %format!("{e:#}"), "could not record a checkpoint");
                let _ = events
                    .send(AgentEvent::Notice(format!(
                        "could not checkpoint after {tool}: {e:#}; /undo will not reach this change"
                    )))
                    .await;
            }
        }
    }

    /// Turn a completion into the one shape the loop reads, whichever protocol produced it.
    async fn read_turn(
        &mut self,
        completion: Completion,
        schema: &Value,
        validator: &Validator,
        cancel: &CancellationToken,
        events: &mpsc::Sender<AgentEvent>,
    ) -> Result<ModelTurn> {
        match self.cfg.protocol {
            ToolProtocol::Native => {
                let mut calls = Vec::new();
                for (id, name, arguments) in completion.calls.finish() {
                    // The arguments come over as a string of JSON the model wrote, so they
                    // are wrong in the same ways the envelope's are, and earn the same
                    // tolerant parse. What is not repaired here is the name: a call to a
                    // tool that does not exist is refused by `dispatch_tool`, which tells
                    // the model so and lets it try again.
                    let arguments = repair::tolerant_parse(&arguments).unwrap_or(Value::Null);
                    calls.push(ResolvedCall {
                        // A server that omits the id leaves us to invent one; it will only
                        // ever be matched against the result we return for it.
                        id: id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                        name,
                        arguments,
                    });
                }
                Ok(ModelTurn {
                    text: completion.text,
                    calls,
                })
            }
            ToolProtocol::Envelope => {
                let _ = events.send(AgentEvent::Phase(Phase::Repairing)).await;
                let envelope = self
                    .parse_with_repair(&completion.text, schema, validator, cancel)
                    .await?;
                Ok(ModelTurn {
                    text: envelope.assistant_text,
                    calls: envelope
                        .tool_calls
                        .into_iter()
                        .map(|tc| ResolvedCall {
                            id: uuid::Uuid::new_v4().to_string(),
                            name: tc.name,
                            arguments: tc.arguments,
                        })
                        .collect(),
                })
            }
        }
    }

    /// Put the assistant's turn into the history in the form the protocol expects to see
    /// it again.
    ///
    /// The model is shown its own previous turn on the next request, so it has to be
    /// recorded the way that protocol reads: `tool_calls` on the message natively, and the
    /// serialised envelope otherwise. Recording it in the other one's shape is how a model
    /// ends up unable to see what it just asked for.
    fn record_assistant_turn(&mut self, turn: &ModelTurn) -> Result<()> {
        match self.cfg.protocol {
            ToolProtocol::Native if !turn.calls.is_empty() => {
                let calls = turn
                    .calls
                    .iter()
                    .map(|c| ToolCall {
                        id: c.id.clone(),
                        kind: "function".into(),
                        function: FunctionCall {
                            name: c.name.clone(),
                            arguments: serde_json::to_string(&c.arguments)
                                .unwrap_or_else(|_| "{}".into()),
                        },
                    })
                    .collect();
                self.session
                    .push(Message::assistant_calling(turn.text.clone(), calls));
            }
            ToolProtocol::Native => {
                self.session.push(Message::assistant(turn.text.clone()));
            }
            ToolProtocol::Envelope => {
                let serialised = serde_json::to_string(&serde_json::json!({
                    "assistant_text": turn.text,
                    "tool_calls": turn.calls.iter().map(|c| serde_json::json!({
                        "name": c.name,
                        "arguments": c.arguments,
                    })).collect::<Vec<_>>(),
                }))?;
                self.session.push(Message::assistant(serialised));
            }
        }
        Ok(())
    }

    async fn stream_one(
        &self,
        schema: &Value,
        cancel: &CancellationToken,
        events: &mpsc::Sender<AgentEvent>,
    ) -> Result<Completion> {
        let native = self.cfg.protocol == ToolProtocol::Native;
        // The grammar constrains the envelope. Under the native protocol there is no
        // envelope to constrain, and forcing this schema would demand an object the model
        // is not being asked to produce.
        // Asked for only when the provider can actually hold the model to it. A server
        // that cannot enforce a schema is not sent one: the request would be accepted and
        // the constraint quietly not applied, which is worse than not asking.
        let grammar = match (
            native,
            self.cfg.grammar_enabled && self.backend.capabilities().grammar,
        ) {
            (false, true) => Some(schema.clone()),
            _ => None,
        };

        let req = TurnRequest {
            model: self.cfg.model.clone(),
            messages: self.session.history.clone(),
            tools: native.then(|| tool_specs(&self.registry)),
            grammar,
            max_tokens: self.cfg.max_answer_tokens as u32,
            temperature: 0.2,
            user: Some(self.session.id.clone()),
        };

        let mut stream: ChunkStream = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(anyhow::anyhow!("interrupted")),
            r = self.backend.stream(req) => r?,
        };

        let mut out = Completion::default();
        // Said once each: the phase is what the status line holds until the next one, not
        // a line per token.
        let (mut answering, mut thinking) = (false, false);
        loop {
            let next = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(anyhow::anyhow!("interrupted")),
                // A server that stops speaking mid-answer would otherwise hold the turn
                // until the client's own timeout, which is fifteen minutes of a interface
                // that says it is busy and is not.
                _ = tokio::time::sleep(STREAM_IDLE_TIMEOUT) => {
                    return Err(anyhow::anyhow!(
                        "backend sent nothing for {}s",
                        STREAM_IDLE_TIMEOUT.as_secs()
                    ))
                }
                p = stream.next() => match p {
                    Some(p) => p,
                    None => break,
                },
            };
            match next? {
                StreamEvent::Delta(p) => {
                    if !answering {
                        answering = true;
                        let _ = events.send(AgentEvent::Phase(Phase::Answering)).await;
                    }
                    out.text.push_str(&p);
                    let _ = events.send(AgentEvent::AssistantDelta(p)).await;
                }
                // Shown as it arrives and then dropped. It is deliberately not appended to
                // `out.text`: the scratchpad is not the answer, and replaying it into the
                // history would teach the next turn that it was.
                StreamEvent::Reasoning(p) => {
                    if !answering && !thinking {
                        thinking = true;
                        let _ = events.send(AgentEvent::Phase(Phase::Thinking)).await;
                    }
                    let _ = events.send(AgentEvent::Reasoning(p)).await;
                }
                StreamEvent::Call(delta) => out.calls.push(delta),
                StreamEvent::End {
                    finish_reason,
                    usage,
                } => {
                    out.finish_reason = finish_reason.or(out.finish_reason);
                    out.usage = usage.or(out.usage);
                }
            }
        }
        Ok(out)
    }

    async fn parse_with_repair(
        &mut self,
        raw: &str,
        schema: &Value,
        validator: &Validator,
        cancel: &CancellationToken,
    ) -> Result<TurnEnvelope> {
        // Layer 2: tolerant parse + schema validate.
        let mut last_err: anyhow::Error;
        let mut current = raw.to_string();

        for attempt in 0..=MAX_REPAIR_ATTEMPTS {
            match repair::tolerant_parse(&current) {
                Ok(parsed) => {
                    if validator.is_valid(&parsed) {
                        let env: TurnEnvelope = serde_json::from_value(parsed)?;
                        return Ok(env);
                    }
                    let errors: Vec<String> = validator
                        .iter_errors(&parsed)
                        .map(|e| format!("{}", e))
                        .take(5)
                        .collect();
                    last_err = anyhow::anyhow!("schema violations: {}", errors.join("; "));
                }
                Err(e) => last_err = e,
            }

            if attempt >= MAX_REPAIR_ATTEMPTS {
                return Err(last_err);
            }

            // Layer 3: repair round-trip (non-streaming for snappiness).
            tracing::warn!(
                attempt = attempt + 1,
                max = MAX_REPAIR_ATTEMPTS,
                why = %last_err,
                "asking the model to repair its own output"
            );
            let prompt = repair::repair_prompt(&last_err.to_string(), &current);
            let repair_req = TurnRequest {
                // A malformed reply has to be reshaped, not thought about, so this is the
                // one call in the loop whose model can be the smallest one available.
                model: self.model_for(crate::agent::roles::Role::Repair),
                messages: vec![
                    Message::system("You are a strict JSON formatter. Output a single JSON object only — no prose, no fences."),
                    Message::user(prompt),
                ],
                // The repair round-trip asks for one thing: valid JSON. Offering tools
                // here would invite the model to do something else instead.
                tools: None,
                grammar: self.backend.capabilities().grammar.then(|| schema.clone()),
                max_tokens: 4096,
                temperature: 0.0,
                user: Some(format!("{}-repair", self.session.id)),
            };
            current = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(anyhow::anyhow!("interrupted")),
                r = self.backend.complete(repair_req) => r?,
            };
        }
        unreachable!()
    }

    async fn dispatch_tool(&self, name: &str, args: Value) -> ToolOutput {
        let Some(tool) = self.registry.get(name) else {
            tracing::warn!(tool = name, "model called a tool that does not exist");
            return ToolOutput::err(format!("unknown tool '{name}'"));
        };
        // Refused here rather than at the approval prompt: planning is a question about
        // what would happen, and answering it must not depend on somebody declining
        // fifteen times. The refusal names the mode so the model plans instead of
        // retrying, and reads as this agent's rule rather than as the user saying no.
        if self.cfg.mode == Mode::Plan && tool.mutates() {
            tracing::info!(tool = name, "refused while planning");
            return ToolOutput::err(format!(
                "planning only: '{name}' would change something, so it was not run. \
                 Say what you would do with it as a step of the plan."
            ));
        }
        // Only around a call that changes something: a hook on every read would run on
        // the searches an agent does by the dozen, and there is nothing there to check.
        if tool.mutates() {
            if let Some(hook) = self.cfg.hooks.pre_tool.clone() {
                let ran = crate::agent::hooks::run(&hook, &self.ctx.root, name, "pre-tool").await;
                if !ran.ok {
                    return ToolOutput::err(format!(
                        "refused before it ran, by this project's pre-tool hook:\n{}",
                        ran.output
                    ));
                }
            }
        }
        let started = std::time::Instant::now();
        let out = match tool.run(&self.ctx, args).await {
            Ok(o) => o,
            Err(e) => ToolOutput::err(format!("tool '{name}' failed: {e:#}")),
        };
        // What the compiler already knows about the file that was just written, handed
        // back in the same result. Without it the model learns of its own syntax error on
        // the next turn, if it thinks to ask, and usually it does not.
        let out = match self.verify_written(name, &out).await {
            Some(diagnostics) => {
                let mut out = out;
                out.content
                    .push_str(&format!("\n\n{CHECK_FAILED}\n{diagnostics}"));
                out
            }
            None => out,
        };
        if tool.mutates() {
            if let Some(hook) = self.cfg.hooks.post_tool.clone() {
                let ran = crate::agent::hooks::run(&hook, &self.ctx.root, name, "post-tool").await;
                // What it says is handed over whether it succeeded or not: a formatter
                // that rewrote the file has something the model needs to know.
                if !ran.output.is_empty() {
                    let mut out = out;
                    out.content
                        .push_str(&format!("\n\npost-tool hook:\n{}", ran.output));
                    return out;
                }
            }
        }
        tracing::info!(
            tool = name,
            ms = started.elapsed().as_millis(),
            error = out.is_error,
            out_chars = out.content.len(),
            "tool finished"
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::fs_read::ListDir;
    use crate::tools::harness::{Answer, Harness};
    use std::sync::{Arc as StdArc, Mutex};

    /// A backend that answers a queue of canned responses, and remembers what it was sent.
    ///
    /// Enough of a server to drive the loop through a real request, a real stream and a
    /// real second step - which is where the protocols actually differ.
    struct FakeBackend {
        url: String,
        seen: StdArc<Mutex<Vec<serde_json::Value>>>,
    }

    async fn fake_backend(responses: Vec<String>) -> FakeBackend {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen: StdArc<Mutex<Vec<serde_json::Value>>> = StdArc::new(Mutex::new(Vec::new()));
        let recorder = seen.clone();

        tokio::spawn(async move {
            let mut queue = responses.into_iter();
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                // Read until the body has arrived; the request declares its length.
                while let Ok(n) = sock.read(&mut chunk).await {
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).into_owned();
                    let Some(head_end) = text.find("\r\n\r\n") else {
                        continue;
                    };
                    let len: usize = text
                        .lines()
                        .find_map(|l| {
                            l.strip_prefix("content-length: ")
                                .or_else(|| l.strip_prefix("Content-Length: "))
                        })
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                    if buf.len() >= head_end + 4 + len {
                        if let Ok(v) = serde_json::from_str(&text[head_end + 4..]) {
                            recorder.lock().unwrap().push(v);
                        }
                        break;
                    }
                }
                let response = queue.next().unwrap_or_else(|| {
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n".into()
                });
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.flush().await;
                let _ = sock.shutdown().await;
            }
        });

        FakeBackend {
            url: format!("http://127.0.0.1:{port}"),
            seen,
        }
    }

    fn sse(events: &[&str]) -> String {
        let body: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
        let body = format!("{body}data: [DONE]\n\n");
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    fn rejects_tools() -> String {
        let body =
            r#"{"error":{"message":"unknown field `tools`","type":"invalid_request_error"}}"#;
        format!(
            "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    fn agent_for(h: &Harness, backend: &FakeBackend, protocol: ToolProtocol) -> Agent {
        let mut reg = Registry::new();
        reg.register(ListDir);
        agent_with(h, backend, protocol, reg)
    }

    /// The same, with every built-in tool, for cases that need one to actually do something.
    fn agent_with_all_tools(h: &Harness, backend: &FakeBackend, protocol: ToolProtocol) -> Agent {
        let reg = crate::boot::build_registry(h.root.clone()).0;
        agent_with(h, backend, protocol, reg)
    }

    fn agent_with(
        h: &Harness,
        backend: &FakeBackend,
        protocol: ToolProtocol,
        reg: Registry,
    ) -> Agent {
        agent_steerable(h, backend, protocol, reg).0
    }

    /// The same, keeping the end of the steering channel a test needs to type into.
    fn agent_steerable(
        h: &Harness,
        backend: &FakeBackend,
        protocol: ToolProtocol,
        reg: Registry,
    ) -> (Agent, mpsc::Sender<String>) {
        let registry = Arc::new(reg);
        let (steer_tx, steer_rx) = mpsc::channel::<String>(8);
        let agent = Agent {
            history: Default::default(),
            escalated_from: None,
            clean_steps: 0,
            warming: Default::default(),
            spend: Spend::default(),
            steps: Vec::new(),
            lsp: None,
            steer: steer_rx,
            backend: Arc::new(crate::backend::client::BackendClient::new(
                backend.url.clone(),
            )),
            session: Session::new(
                Agent::build_system_prompt(&registry, protocol, Mode::Act, None),
                8192,
                4096,
            ),
            registry,
            cfg: AgentConfig {
                mode: Mode::Act,
                preload: false,
                roles: Default::default(),
                hooks: Default::default(),
                instructions: None,
                // A test must never write into the person's own configuration.
                remember_model_in: None,
                context_tokens: 8192,
                models: Default::default(),
                model: "test".into(),
                grammar_enabled: true,
                max_steps: 4,
                max_answer_tokens: 256,
                protocol,
                allow_fallback: true,
            },
            ctx: h.ctx.clone(),
            checkpoints: None,
            turn: 0,
        };
        (agent, steer_tx)
    }

    async fn drive(agent: &mut Agent, prompt: &str) -> Vec<AgentEvent> {
        let (tx, mut rx) = mpsc::channel(64);
        agent
            .run_user_turn(prompt.into(), CancellationToken::new(), tx)
            .await
            .unwrap();
        let mut out = Vec::new();
        while let Ok(e) = rx.try_recv() {
            out.push(e);
        }
        out
    }

    /// Planning is long and ends in acting, so the model acting will use is asked for
    /// while the plan is being written. The request goes out and the turn does not wait
    /// on it: warming that delayed anything would cost more than the load it hides.
    #[tokio::test]
    async fn planning_asks_for_the_model_acting_will_use() {
        let h = Harness::new(Answer::Yes);
        let backend = fake_backend(vec![sse(&[
            r#"{"choices":[{"delta":{"content":"ok"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        ])])
        .await;
        let mut agent = agent_for(&h, &backend, ToolProtocol::Native);
        agent.cfg.preload = true;
        agent.cfg.roles.edit = Some("the-coder".into());

        let said = agent.set_mode(Mode::Plan);
        assert!(said.starts_with("planning"), "{said}");
        // The warming request is its own task; it has arrived by the time the fake
        // backend has seen a second request.
        for _ in 0..40 {
            if backend
                .seen
                .lock()
                .unwrap()
                .iter()
                .any(|r| r["model"] == "the-coder")
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("nothing was asked of the model acting would use");
    }

    /// A delegated question runs in a session of its own and brings back only what it
    /// said: the caller's window holds the answer, not the search that found it.
    #[tokio::test]
    async fn a_delegated_question_returns_its_answer_and_not_its_search() {
        let h = Harness::new(Answer::Yes);
        h.write("a.txt", "the answer is forty two");
        let backend = fake_backend(vec![
            // The caller delegates.
            sse(&[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"delegate","arguments":"{\"task\": \"what does a.txt say\"}"}}]}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ]),
            // The sub-session reads the file.
            sse(&[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"s1","function":{"name":"read_file","arguments":"{\"path\": \"a.txt\"}"}}]}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ]),
            // The sub-session answers.
            sse(&[
                r#"{"choices":[{"delta":{"content":"it says forty two"}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ]),
            // The caller answers.
            sse(&[
                r#"{"choices":[{"delta":{"content":"forty two"}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ]),
        ])
        .await;

        let mut reg = crate::boot::build_registry(h.root.clone()).0;
        let sub = Arc::new(reg.clone());
        reg.register(crate::tools::delegate::Delegate {
            what: Arc::new(crate::tools::delegate::Delegation {
                backend: Arc::new(crate::backend::client::BackendClient::new(
                    backend.url.clone(),
                )),
                registry: sub,
                model: "test".into(),
                roles: Default::default(),
                context_tokens: 8192,
                max_answer_tokens: 4096,
                protocol: ToolProtocol::Native,
                grammar_enabled: false,
                max_steps: 4,
            }),
        });
        let mut agent = agent_with(&h, &backend, ToolProtocol::Native, reg);
        let events = drive(&mut agent, "what does a.txt say").await;

        let result = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::ToolResult { name, output } if name == "delegate" => Some(output),
                _ => None,
            })
            .expect("the delegation was never answered");
        assert!(!result.is_error, "{result:?}");
        assert_eq!(result.content, "it says forty two");
        // What the sub-session read is not in the caller's conversation.
        let history: String = agent
            .session
            .history
            .iter()
            .map(|m| m.content.clone())
            .collect();
        assert!(
            !history.contains("the answer is forty two"),
            "the search came back with the answer"
        );
    }

    /// A pre-tool hook that refuses stops the call, and what it printed is what the model
    /// is told: guidance an agent cannot skip, which is the only kind that holds.
    #[tokio::test]
    async fn a_refusing_hook_stops_the_tool_and_is_handed_to_the_model() {
        let h = Harness::new(Answer::Yes);
        h.write("a.txt", "one");
        let backend = fake_backend(vec![
            sse(&[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"write_file","arguments":"{\"path\": \"a.txt\", \"content\": \"two\"}"}}]}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ]),
            sse(&[
                r#"{"choices":[{"delta":{"content":"understood"}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ]),
        ])
        .await;

        let mut agent = agent_with_all_tools(&h, &backend, ToolProtocol::Native);
        agent.cfg.hooks.pre_tool = Some("echo the tree is not clean >&2; exit 1".into());
        let events = drive(&mut agent, "change it").await;

        let refusal = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::ToolResult { name, output } if name == "write_file" => Some(output),
                _ => None,
            })
            .expect("the write was never answered");
        assert!(refusal.is_error);
        assert!(
            refusal.content.contains("the tree is not clean"),
            "{refusal:?}"
        );
        assert_eq!(h.read("a.txt"), "one");
    }

    /// A hook at the end of a turn is for the person: it says what it did, and it runs
    /// even for a turn that was abandoned, because that turn can have changed files.
    #[tokio::test]
    async fn the_turn_end_hook_reports_to_the_user() {
        let h = Harness::new(Answer::Yes);
        let backend = fake_backend(vec![sse(&[
            r#"{"choices":[{"delta":{"content":"nothing to do"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        ])])
        .await;
        let mut agent = agent_for(&h, &backend, ToolProtocol::Native);
        agent.cfg.hooks.turn_end = Some("echo formatted 3 files".into());
        let events = drive(&mut agent, "hello").await;
        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentEvent::Notice(s) if s.contains("formatted 3 files")
            )),
            "the hook said nothing: {events:?}"
        );
    }

    /// While planning, a tool that would change something is refused before it runs, and
    /// the refusal says why so the model writes a plan instead of calling again. A tool
    /// that only reads still runs: a plan nobody could research would be worth nothing.
    #[tokio::test]
    async fn planning_refuses_what_would_change_something_and_still_reads() {
        let h = Harness::new(Answer::Yes);
        h.write("a.txt", "one");
        let backend = fake_backend(vec![
            sse(&[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"write_file","arguments":"{\"path\": \"a.txt\", \"content\": \"two\"}"}}]}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ]),
            sse(&[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c2","function":{"name":"read_file","arguments":"{\"path\": \"a.txt\"}"}}]}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ]),
            sse(&[
                r#"{"choices":[{"delta":{"content":"1. rewrite a.txt"}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ]),
        ])
        .await;

        let mut agent = agent_with_all_tools(&h, &backend, ToolProtocol::Native);
        agent.cfg.mode = Mode::Plan;
        agent.rebuild_preamble();
        let events = drive(&mut agent, "change a.txt to two").await;

        let refusal = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::ToolResult { name, output } if name == "write_file" => Some(output),
                _ => None,
            })
            .expect("the write was never answered");
        assert!(refusal.is_error);
        assert!(refusal.content.contains("planning only"), "{refusal:?}");
        // The file is what it was: the refusal is before the tool, not inside it.
        assert_eq!(h.read("a.txt"), "one");
        // Reading is how a plan gets written, so it is not refused.
        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentEvent::ToolResult { name, output } if name == "read_file" && !output.is_error
            )),
            "reading was refused too: {events:?}"
        );
        // The model is told it is planning, so it stops calling and writes the plan.
        assert!(agent.session.history[0]
            .content
            .contains("You are PLANNING"));
    }

    /// A switch takes the window the configuration gives that model, and a name the
    /// backend does not serve is refused rather than carried into every later request.
    #[tokio::test]
    async fn switching_model_takes_its_window_and_refuses_an_unserved_name() {
        let h = Harness::new(Answer::Yes);
        let backend = fake_backend(vec![]).await;
        let mut agent = agent_for(&h, &backend, ToolProtocol::Native);
        agent.cfg.models.insert(
            "big".into(),
            crate::config::ModelSection {
                context_tokens: Some(32_768),
                max_answer_tokens: None,
            },
        );
        let before = agent.session.budget_chars();

        // The fake backend answers nothing on /v1/models, so a name is taken on trust:
        // a backend that cannot list is not a backend that cannot serve.
        let said = agent.switch_model(Some("big".into())).await;
        assert!(said.contains("now on big"), "{said}");
        assert_eq!(agent.cfg.model, "big");
        assert!(
            agent.session.budget_chars() > before,
            "the wider window was not taken: {} then {}",
            before,
            agent.session.budget_chars()
        );

        let said = agent.switch_model(Some("big".into())).await;
        assert!(said.contains("already on big"), "{said}");
    }

    /// A line typed while the turn is running reaches the model on the next step, and
    /// says so. Without this the only interventions are abandoning the turn or waiting
    /// for it to end, which is restarting rather than steering.
    #[tokio::test]
    async fn a_line_typed_during_a_turn_joins_the_next_step() {
        let h = Harness::new(Answer::Yes);
        h.write("a.txt", "");
        let backend = fake_backend(vec![
            sse(&[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"list_dir","arguments":"{\"path\": \".\"}"}}]}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ]),
            sse(&[
                r#"{"choices":[{"delta":{"content":"done"}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ]),
        ])
        .await;

        let reg = crate::boot::build_registry(h.root.clone()).0;
        let (mut agent, steer) = agent_steerable(&h, &backend, ToolProtocol::Native, reg);
        // Queued before the turn is driven: the drain is at the step boundary, so it is
        // read on the step it precedes rather than after the turn.
        steer
            .send("only the .txt files, please".into())
            .await
            .unwrap();
        let events = drive(&mut agent, "what is here").await;

        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Steered(s) if s.contains("only the .txt"))),
            "the turn never said it had taken the line: {events:?}"
        );
        let sent = backend.seen.lock().unwrap();
        let messages = sent[0]["messages"].as_array().unwrap();
        assert!(
            messages.iter().any(|m| m["role"] == "user"
                && m["content"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("only the .txt")),
            "the model was never told: {messages:?}"
        );
    }

    /// The native path end to end: a streamed call is reassembled, dispatched, and
    /// answered against the id the server issued.
    #[tokio::test]
    async fn a_native_call_is_run_and_answered_against_its_own_id() {
        let h = Harness::new(Answer::Yes);
        h.write("a.txt", "");
        let backend = fake_backend(vec![
            sse(&[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_abc","function":{"name":"list_dir","arguments":"{\"path\""}}]}}]}"#,
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":": \".\"}"}}]}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ]),
            sse(&[
                r#"{"choices":[{"delta":{"content":"there is one file"}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ]),
        ])
        .await;

        let mut agent = agent_for(&h, &backend, ToolProtocol::Native);
        let events = drive(&mut agent, "what is here").await;

        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolCall { name, .. } if name == "list_dir")),
            "the streamed call was never dispatched: {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::ToolResult { output, .. } if output.content.contains("a.txt"))),
            "the tool did not actually run: {events:?}"
        );

        // The assistant turn carries the call, and the result answers that id - the thing
        // a strict server checks and the old code could never satisfy.
        let assistant = agent
            .session
            .history
            .iter()
            .find(|m| m.role == "assistant" && m.tool_calls.is_some())
            .expect("no assistant turn carried tool_calls");
        let calls = assistant.tool_calls.as_ref().unwrap();
        assert_eq!(calls[0].id, "call_abc");
        assert_eq!(calls[0].function.name, "list_dir");

        let answer = agent
            .session
            .history
            .iter()
            .find(|m| m.role == "tool")
            .expect("no tool result recorded");
        assert_eq!(answer.tool_call_id.as_deref(), Some("call_abc"));
    }

    /// The tools have to actually reach the server, or the model is being asked to call
    /// things it has not been told about.
    #[tokio::test]
    async fn the_native_request_carries_the_tools() {
        let h = Harness::new(Answer::Yes);
        let backend = fake_backend(vec![sse(&[
            r#"{"choices":[{"delta":{"content":"nothing to do"},"finish_reason":"stop"}]}"#,
        ])])
        .await;

        let mut agent = agent_for(&h, &backend, ToolProtocol::Native);
        drive(&mut agent, "hello").await;

        let seen = backend.seen.lock().unwrap();
        let req = seen.first().expect("no request reached the backend");
        assert_eq!(req["tools"][0]["function"]["name"], "list_dir");
        assert_eq!(req["tool_choice"], "auto");
        assert!(
            req["response_format"].is_null(),
            "the envelope grammar was imposed on a native request: {req}"
        );
    }

    /// A backend that refuses the field must not cost the user their turn.
    #[tokio::test]
    async fn a_backend_that_refuses_tools_falls_back_and_finishes_the_turn() {
        let h = Harness::new(Answer::Yes);
        let backend = fake_backend(vec![
            rejects_tools(),
            sse(&[
                r#"{"choices":[{"delta":{"content":"{\"assistant_text\":\"done\",\"tool_calls\":[]}"},"finish_reason":"stop"}]}"#,
            ]),
        ])
        .await;

        let mut agent = agent_for(&h, &backend, ToolProtocol::Native);
        let events = drive(&mut agent, "hello").await;

        assert_eq!(
            agent.cfg.protocol,
            ToolProtocol::Envelope,
            "no fallback happened"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::AssistantText(t) if t == "done")),
            "the turn did not complete after falling back: {events:?}"
        );

        // The preamble has to change with the protocol, or the model is reading the
        // contract for the other one.
        assert!(
            agent.session.history[0].content.contains("assistant_text"),
            "the system message still describes the native protocol"
        );

        let seen = backend.seen.lock().unwrap();
        assert!(
            seen[0]["tools"].is_array(),
            "the first attempt was not native"
        );
        assert!(seen[1]["tools"].is_null(), "the retry was still native");
    }

    fn call(name: &str) -> ResolvedCall {
        ResolvedCall {
            id: name.into(),
            name: name.into(),
            arguments: serde_json::json!({}),
        }
    }

    fn registry_of_all_tools() -> Registry {
        crate::boot::build_registry(std::env::temp_dir()).0
    }

    fn shape(runs: &[Vec<ResolvedCall>]) -> Vec<Vec<&str>> {
        runs.iter()
            .map(|r| r.iter().map(|c| c.name.as_str()).collect())
            .collect()
    }

    /// Reads that cannot see each other go together.
    #[test]
    fn independent_reads_form_one_run() {
        let reg = registry_of_all_tools();
        let grouped = runs(
            &reg,
            vec![call("read_file"), call("grep"), call("list_dir")],
        );
        assert_eq!(shape(&grouped), vec![vec!["read_file", "grep", "list_dir"]]);
    }

    /// A write is a boundary on both sides: what it changes is what the next read would
    /// see, and the checkpoint after it has to describe that call alone.
    #[test]
    fn a_write_stands_alone_between_the_reads_around_it() {
        let reg = registry_of_all_tools();
        let grouped = runs(
            &reg,
            vec![
                call("read_file"),
                call("grep"),
                call("write_file"),
                call("read_file"),
                call("glob"),
            ],
        );
        assert_eq!(
            shape(&grouped),
            vec![
                vec!["read_file", "grep"],
                vec!["write_file"],
                vec!["read_file", "glob"],
            ]
        );
    }

    /// Two writes must not be grouped, or one checkpoint would cover both.
    #[test]
    fn writes_are_never_grouped() {
        let reg = registry_of_all_tools();
        let grouped = runs(
            &reg,
            vec![call("write_file"), call("edit_file"), call("bash")],
        );
        assert_eq!(
            shape(&grouped),
            vec![vec!["write_file"], vec!["edit_file"], vec!["bash"]]
        );
    }

    /// A tool that does not exist is answered with a refusal, and grouping it would only
    /// obscure which call was refused.
    #[test]
    fn an_unknown_tool_stands_alone() {
        let reg = registry_of_all_tools();
        let grouped = runs(
            &reg,
            vec![call("read_file"), call("no_such_tool"), call("grep")],
        );
        assert_eq!(
            shape(&grouped),
            vec![vec!["read_file"], vec!["no_such_tool"], vec!["grep"]]
        );
    }

    /// Reads that run together still answer in the order they were asked, whatever order
    /// they finish in - a result is matched to its call by position and by id.
    #[tokio::test]
    async fn parallel_reads_answer_in_the_order_they_were_asked() {
        let h = Harness::new(Answer::Yes);
        h.write("a.txt", "AAA");
        h.write("b.txt", "BBB");
        let backend = fake_backend(vec![
            sse(&[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"read_file","arguments":"{\"path\":\"a.txt\"}"}},{"index":1,"id":"c2","function":{"name":"read_file","arguments":"{\"path\":\"b.txt\"}"}}]}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ]),
            sse(&[
                r#"{"choices":[{"delta":{"content":"read both"}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ]),
        ])
        .await;

        let mut agent = agent_with_all_tools(&h, &backend, ToolProtocol::Native);
        drive(&mut agent, "read them").await;

        let answers: Vec<_> = agent
            .session
            .history
            .iter()
            .filter(|m| m.role == "tool")
            .map(|m| {
                (
                    m.tool_call_id.clone().unwrap_or_default(),
                    m.content.clone(),
                )
            })
            .collect();
        assert_eq!(answers.len(), 2, "{answers:?}");
        assert_eq!(answers[0].0, "c1");
        assert!(answers[0].1.contains("AAA"), "{answers:?}");
        assert_eq!(answers[1].0, "c2");
        assert!(answers[1].1.contains("BBB"), "{answers:?}");
    }

    /// The envelope path must keep working exactly as it did.
    #[tokio::test]
    async fn the_envelope_protocol_still_drives_a_turn() {
        let h = Harness::new(Answer::Yes);
        h.write("a.txt", "");
        let backend = fake_backend(vec![
            sse(&[
                r#"{"choices":[{"delta":{"content":"{\"assistant_text\":\"looking\",\"tool_calls\":[{\"name\":\"list_dir\",\"arguments\":{\"path\":\".\"}}]}"},"finish_reason":"stop"}]}"#,
            ]),
            sse(&[
                r#"{"choices":[{"delta":{"content":"{\"assistant_text\":\"done\",\"tool_calls\":[]}"},"finish_reason":"stop"}]}"#,
            ]),
        ])
        .await;

        let mut agent = agent_for(&h, &backend, ToolProtocol::Envelope);
        let events = drive(&mut agent, "what is here").await;

        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::ToolResult { output, .. } if output.content.contains("a.txt"))),
            "{events:?}"
        );
        let seen = backend.seen.lock().unwrap();
        assert!(
            seen[0]["tools"].is_null(),
            "the envelope request offered tools"
        );
        assert!(
            !seen[0]["response_format"].is_null(),
            "the grammar was not applied"
        );
    }

    use crate::agent::steps::{After, Before, StepRecord};

    fn step(check: Option<bool>) -> StepRecord {
        StepRecord {
            turn: 1,
            model: "small".into(),
            before: Before::default(),
            after: After {
                check_passed: check,
                wrote: check.is_some(),
                ..Default::default()
            },
        }
    }

    async fn agent_at(model: &str) -> (Agent, mpsc::Receiver<AgentEvent>) {
        let h = Harness::new(Answer::Yes);
        let backend = fake_backend(vec![]).await;
        let mut agent = agent_for(&h, &backend, ToolProtocol::Native);
        agent.cfg.model = model.into();
        agent.cfg.roles.escalate = Some("big".into());
        let (_tx, rx) = mpsc::channel(16);
        (agent, rx)
    }

    /// The whole point: the model is raised because a compiler said the change was wrong,
    /// not because anything guessed that the task looked hard.
    #[tokio::test]
    async fn a_failed_check_raises_the_model_and_two_clean_steps_lower_it() {
        let (mut agent, _rx) = agent_at("small").await;
        let (tx, _keep) = mpsc::channel(16);

        agent.reconsider_model(&step(Some(true)), &tx).await;
        assert_eq!(agent.cfg.model, "small", "a clean step raised the model");

        agent.reconsider_model(&step(Some(false)), &tx).await;
        assert_eq!(agent.cfg.model, "big");
        assert_eq!(agent.escalated_from.as_deref(), Some("small"));

        // One clean step is not enough: the next thing it writes may be the rest of the
        // same mistake.
        agent.reconsider_model(&step(Some(true)), &tx).await;
        assert_eq!(agent.cfg.model, "big");
        agent.reconsider_model(&step(Some(true)), &tx).await;
        assert_eq!(agent.cfg.model, "small");
        assert!(agent.escalated_from.is_none());
    }

    /// What the project taught is applied once, at the start of a turn, so a session does
    /// not pay again for the bad step the last one already paid for.
    #[tokio::test]
    async fn a_turn_starts_raised_when_the_cheap_model_keeps_failing_here() {
        let h = Harness::new(Answer::Yes);
        let backend = fake_backend(vec![sse(&[
            r#"{"choices":[{"delta":{"content":"ok"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        ])])
        .await;
        let mut agent = agent_for(&h, &backend, ToolProtocol::Native);
        agent.cfg.model = "small".into();
        agent.cfg.roles.escalate = Some("big".into());
        for _ in 0..4 {
            agent.history.record("small", false);
        }

        let events = drive(&mut agent, "change something").await;
        assert_eq!(agent.cfg.model, "big");
        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentEvent::Notice(s) if s.contains("failing its writes here")
            )),
            "the turn did not say why it started where it did: {events:?}"
        );
        // What it was raised from is remembered, so two clean writes bring it back.
        assert_eq!(agent.escalated_from.as_deref(), Some("small"));
    }

    /// One rung. A second failure while already raised does not look for a third model,
    /// because nothing here knows whether a third would help.
    #[tokio::test]
    async fn a_second_failure_does_not_raise_again() {
        let (mut agent, _rx) = agent_at("small").await;
        let (tx, _keep) = mpsc::channel(16);
        agent.reconsider_model(&step(Some(false)), &tx).await;
        agent.reconsider_model(&step(Some(false)), &tx).await;
        assert_eq!(agent.cfg.model, "big");
        assert_eq!(agent.escalated_from.as_deref(), Some("small"));
    }

    /// Nothing writes while planning, so nothing there can have failed a check.
    #[tokio::test]
    async fn planning_never_raises() {
        let (mut agent, _rx) = agent_at("small").await;
        agent.cfg.mode = Mode::Plan;
        let (tx, _keep) = mpsc::channel(16);
        agent.reconsider_model(&step(Some(false)), &tx).await;
        assert_eq!(agent.cfg.model, "small");
    }

    /// A step that changed nothing says nothing about the model, so it neither raises nor
    /// counts towards coming back down.
    #[tokio::test]
    async fn a_step_that_wrote_nothing_moves_nothing() {
        let (mut agent, _rx) = agent_at("small").await;
        let (tx, _keep) = mpsc::channel(16);
        agent.reconsider_model(&step(Some(false)), &tx).await;
        assert_eq!(agent.cfg.model, "big");
        agent.reconsider_model(&step(None), &tx).await;
        agent.reconsider_model(&step(None), &tx).await;
        assert_eq!(agent.cfg.model, "big", "reads brought it down");
    }
}

#[cfg(test)]
mod spend_tests {
    use super::Spend;

    #[test]
    fn a_session_that_has_done_nothing_says_so() {
        assert_eq!(
            Spend::default().describe(),
            "nothing spent yet this session"
        );
    }

    /// The rate is the answer's tokens over the time spent waiting for the model, not
    /// over the whole turn: time in a tool is not the model being slow.
    #[test]
    fn the_rate_is_measured_against_the_time_in_the_model() {
        let s = Spend {
            prompt_tokens: 4000,
            completion_tokens: 200,
            steps: 3,
            model_ms: 10_000,
            tool_ms: 5_000,
        };
        let said = s.describe();
        assert!(said.contains("3 steps"), "{said}");
        assert!(said.contains("4000 tokens in"), "{said}");
        assert!(said.contains("200 out at 20.0/s"), "{said}");
        assert!(said.contains("10.0s in the model"), "{said}");
        assert!(said.contains("5.0s in tools"), "{said}");
    }
}
