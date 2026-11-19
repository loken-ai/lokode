use std::sync::Arc;

use anyhow::Result;
use futures::StreamExt;
use jsonschema::Validator;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::protocol::{
    calls_in_reasoning, looks_like_a_call, CallAccumulator, ModelTurn, ResolvedCall, ToolProtocol,
    TurnEnvelope,
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
/// A turn spends most of its time where nothing is printed: the request has gone out and the model
/// is reading a context that can be thousands of tokens, or a command is running under a tool.
/// Without a word for those the interface looks identical to one that has stopped, which is what a
/// user reports as "nothing is happening".
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
/// Planning is not a weaker set of approvals: it is a different question.
/// A user who wants to know what would happen should not have to say no to 15 prompts to find out,
/// and an agent that has to ask before every step of a plan cannot describe the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Reads and searches.|Every tool that changes something is refused before it runs.
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
/// The agent runs in its own task and owns the session.
/// A question about the session cannot be answered from the interface's side.
/// Examples are which model answers and how full the window is.
/// It is asked here and answered on the channel the asker holds.
pub enum Control {
    /// Start a tool server again, and take its tools again.
    /// With `spec`, a server the session does not have is started as configured.
    RestartMcp {
        server: String,
        spec: Option<Box<crate::config::McpServer>>,
        reply: tokio::sync::oneshot::Sender<String>,
    },
    /// Switch model, or, with no name, say what could be switched to.
    Model {
        name: Option<String>,
        reply: tokio::sync::oneshot::Sender<String>,
    },
    /// What the backend serves, for choosing from rather than reading.
    Models {
        reply: tokio::sync::oneshot::Sender<Result<Models, String>>,
    },
    /// Start the conversation again, keeping the project and the model.
    Clear {
        reply: tokio::sync::oneshot::Sender<String>,
    },
    /// The window the backend turned out to serve, found out while the session was already on
    /// screen.
    SeeWindow(usize),
    /// Come back to a session recorded earlier, by its id.
    Resume {
        id: String,
        reply: tokio::sync::oneshot::Sender<Result<Resumed, String>>,
    },
    /// Hold the session to a tool protocol, once something has found out which one works.
    Protocol(ToolProtocol),
    /// Take the conversation back to the last thing the user asked and hand it back.
    /// It can then be asked again, of another model or of the same one from a clean start.
    Retry {
        model: Option<String>,
        reply: tokio::sync::oneshot::Sender<Result<String, String>>,
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
    /// What the status line has to show and cannot know.
    /// The model answering now, and how full the window is.
    /// Both can change under the interface, by a command, by the mode or by a failed check.
    /// So the status line asks rather than remembering what it was told at startup.
    Status {
        reply: tokio::sync::oneshot::Sender<StatusReport>,
    },
    /// What the session has spent.
    Spend {
        reply: tokio::sync::oneshot::Sender<String>,
    },
    /// Why this model is the one answering.
    Why {
        reply: tokio::sync::oneshot::Sender<String>,
    },
    /// Something that happened outside the conversation, for the conversation to know about: the
    /// output of a command the person ran themselves.
    ///
    /// Answered between turns like every other request here, so a note never lands in a history a
    /// request is half-built against.
    Note(String),
}

/// Where a tool's answer came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The tool ran for this call.
    Ran,
    /// A read made while the model was still writing, guessed right.
    Guessed,
    /// The same call was already answered this turn, with nothing written since.
    Repeated,
    /// It did not run: a second reading of the change objected first.
    Withheld,
}

/// Events the agent loop emits so the UI can render progress.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// Where the turn is, when nothing else is being printed.
    Phase(Phase),
    AssistantDelta(String),
    AssistantText(String),
    /// The model's scratchpad.
    /// Shown, never recorded, never sent back.
    Reasoning(String),
    ToolCall {
        /// The id the result will carry, so the 2 can be put together.
        id: String,
        name: String,
        args_pretty: String,
    },
    ToolResult {
        id: String,
        name: String,
        output: ToolOutput,
        /// How long the call ran.
        /// Zero for an answer that was already known.
        took: std::time::Duration,
        origin: Origin,
    },
    Notice(String),
    /// What the turn came to, sent as it closes.
    Receipt(Receipt),
    /// The server is reading the prompt, and has got this far, in tokens.
    Prefill {
        done: u64,
        total: u64,
    },
    /// What the backend measured, as of the step just taken.
    Engine(Engine),
    /// A line the user typed while the turn was running, now in the conversation.
    Steered(String),
    /// A question asked from another screen of the same session.
    Asked(String),
    /// A turn the session began by itself, on what an agent working in the background answered
    /// while nothing else ran.
    /// The token stops it.
    Begun(CancellationToken),
    /// How tool calls travel, once something has found out what the backend does.
    Protocol(ToolProtocol),
    /// How full the window is, and with what, as of this step.
    Window(crate::agent::session::Occupancy),
    /// How fast the last step came back, in tokens a second.
    ///
    /// A turn that takes a minute with nothing on screen looks the same as a turn that has stalled.
    /// This is the one number that tells them apart.
    Rate(f64),
    /// What is being done in the background, or nothing once it is done.
    ///
    /// Work that belongs to no turn still takes time.
    /// Holding a model ready is an example.
    /// A screen that says nothing about it looks broken.
    Background(Option<(String, CancellationToken)>),
    /// What the backend serves, for completing the name of one that is being typed.
    Models(Vec<String>),
    /// The session changed mode by itself, as when a plan is approved.
    Mode(Mode),
    TurnDone,
}

/// How fast a step came back, in tokens a second, when the backend said enough to tell.
///
/// Nothing is reported for a step that failed or for a backend that counts nothing: a made up
/// number on a status line is worse than an empty space where one would be.
fn step_rate(attempt: &anyhow::Result<Completion>, model_ms: u64) -> Option<f64> {
    let usage = attempt.as_ref().ok()?.usage?;
    if model_ms == 0 || usage.completion_tokens == 0 {
        return None;
    }
    Some(usage.completion_tokens as f64 * 1000.0 / model_ms as f64)
}

/// The files a write tool says it changed, read from the result rather than the arguments: what was
/// written is what the tool says it wrote, and a call that was refused or failed says nothing to
/// parse.
///
/// The 4 shapes the write tools produce: "wrote P (N bytes)", "edited P", "patched P" once per
/// file, and "applied N edit(s) to P".
fn changed_paths(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in content.lines() {
        let path = if let Some(rest) = line.strip_prefix("wrote ") {
            rest.rsplit_once(" (").map(|(p, _)| p).unwrap_or(rest)
        } else if let Some(rest) = line.strip_prefix("edited ") {
            rest
        } else if let Some(rest) = line.strip_prefix("patched ") {
            rest
        } else if line.starts_with("applied ") {
            match line.split_once(" to ") {
                Some((_, rest)) => rest,
                None => continue,
            }
        } else {
            continue;
        };
        let path = path.trim();
        if !path.is_empty() {
            out.push(path.to_string());
        }
    }
    out
}

use crate::lsp::check::{CHECK_FAILED, CHECK_PASSED, NOT_CHECKED};

/// Left in the transcript when a turn ends without the model having answered.
///
/// The turn's user message is already in the history, and `Session::compact` drops whole steps by
/// looking for user-message boundaries.
/// Leaving a user message with no reply behind it puts 2 `user` roles in a row and moves a boundary
/// the compactor reasons about, so an abandoned turn is closed off rather than left open.
const TURN_ABANDONED: &str = "[this turn ended without an answer]";

/// How long to wait between 2 pieces of a streaming answer before giving up on it.
///
/// Between pieces only.
/// A server that has started speaking and stops has stalled, and the gap says so whatever the
/// prompt was.
const STREAM_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// The slowest prefill worth waiting through, in tokens a second.
///
/// The wait for the first token is the server reading the prompt, and it grows with the prompt.
/// It is not a gap in an answer, so it is not held to the gap allowed between pieces.
/// The figure covers a large model spread over several devices.
/// A prefill slower than it means a server in trouble.
const SLOWEST_PREFILL: usize = 60;

/// How long to wait for the first piece of an answer to a prompt of `chars`.
///
/// Never shorter than the gap allowed between pieces, so a short prompt is judged by the same rule
/// it always was.
fn first_token_timeout(chars: usize) -> std::time::Duration {
    let tokens = chars / crate::agent::session::CHARS_PER_TOKEN;
    std::time::Duration::from_secs((tokens / SLOWEST_PREFILL) as u64).max(STREAM_IDLE_TIMEOUT)
}

/// Split a turn's calls into runs that may go at once.
///
/// A read-only call cannot see what another one did, so several of them are independent and the
/// order they finish in does not matter.
/// A mutating call is a boundary on both sides: what it changes is what a later call would read,
/// and the checkpoint taken after it has to describe that call alone.
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

/// How many steps may fail outright, one after another, before the turn is given back.
///
/// 3 is enough for a search that guesses wrong twice and then reads the tree, and short enough that
/// a model repeating a broken idea costs 3 context reads and not 16.
const BARREN_STEPS: u32 = 3;

/// A guess in flight: the work, and the token that abandons it.
type Guessing = (
    tokio::task::JoinHandle<Option<(String, ToolOutput)>>,
    CancellationToken,
);

/// Everything a guess needs, owned, so it can be abandoned without holding the turn.
struct Guess {
    backend: Arc<dyn crate::backend::provider::Provider>,
    registry: Arc<Registry>,
    ctx: crate::tools::registry::ToolCtx,
    model: String,
    session: String,
    recent: String,
    known: std::collections::HashSet<String>,
}

impl Guess {
    /// Ask which read is coming, make it, and hand back what a repeat of it would find.
    ///
    /// Read-only by construction: a guess that would change something is dropped rather than run,
    /// because the point is that being wrong is free.
    async fn run(self, cancel: CancellationToken) -> Option<(String, ToolOutput)> {
        let names: Vec<&str> = self
            .registry
            .iter()
            .filter(|(_, t)| !t.mutates())
            .map(|(n, _)| n.as_str())
            .collect();
        if names.is_empty() {
            return None;
        }
        let req = TurnRequest {
            model: self.model.clone(),
            messages: vec![
                Message::system(format!(
                    "You are guessing which single read-only call an agent is about to \
                     make next, so its answer can be ready. Reply with one JSON object and \
                     nothing else: {{\"name\": ..., \"arguments\": {{...}}}}. The name must \
                     be one of: {}. Reply with {{}} when you cannot tell.",
                    names.join(", ")
                )),
                Message::user(format!("The conversation so far:\n{}", self.recent)),
            ],
            tools: None,
            grammar: None,
            max_tokens: 160,
            temperature: 0.0,
            user: Some(format!("{}-guess", self.session)),
            reason: false,
            purpose: crate::backend::meter::Purpose::Guess,
        };
        let answer = tokio::select! {
            biased;
            _ = cancel.cancelled() => return None,
            r = self.backend.complete(req) => r.ok()?,
        };
        let parsed = crate::backend::repair::tolerant_parse(&answer).ok()?;
        let name = parsed.get("name")?.as_str()?.to_string();
        let arguments = parsed.get("arguments").cloned().unwrap_or(Value::Null);
        let tool = self.registry.get(&name)?;
        if tool.mutates() {
            return None;
        }
        let key = repeat_key(&ResolvedCall {
            id: String::new(),
            name: name.clone(),
            arguments: arguments.clone(),
        });
        if self.known.contains(&key) {
            return None;
        }
        let out = tokio::select! {
            biased;
            _ = cancel.cancelled() => return None,
            out = tool.run(&self.ctx, arguments) => out.ok()?,
        };
        // A guess that failed teaches the next step nothing and would be served as an answer.
        // Only what worked is worth keeping.
        (!out.is_error).then_some((key, out))
    }
}

/// The one thing a local agent must never do.
///
/// A model used to a chat window asks for the file to be pasted in.
/// Here the project is on this disk and the tools are in its hand, so asking is a turn spent to be
/// told what it could have read, and a person who has to paste their own code has no use for an
/// agent.
const NEVER_ASK: &str = "The project is on this disk and you have tools for it. Never ask \
                         the user to paste, provide or share a file, its contents or its \
                         path: list, search and read it yourself. Ask only about what is \
                         not in the project - what they want, or a decision only they can \
                         make. Paths are relative to the project root, the directory the \
                         session runs in; there is no other root.";

/// How much of a call's arguments a reviewer is shown.
///
/// Enough to see what the change is, bounded because a whole-file write is the whole file and the
/// question is about the shape of the change rather than every line of it.
const REVIEW_ARG_CHARS: usize = 4000;

/// How long an objection may be and still be one.
///
/// A reviewer that writes a paragraph is discussing the change rather than objecting to it, and a
/// paragraph in front of the model is a paragraph it will argue with.
const REVIEW_SAID_CHARS: usize = 400;

/// How many calls may fail in a turn that has changed nothing before it is given back.
///
/// Counted across the turn rather than in a row, because a model that gets its own arguments wrong
/// alternates between calls that work and calls that cannot, and never fails often enough in a row
/// to be stopped by the other rule.
const FAILED_CALLS: u32 = 6;

/// Whether the question is one the project itself answers.
///
/// Only where the person has said so.
/// A question about Rust, or about what a word means, is answered from what the model knows.
/// A question about this code is answered by reading the project.
fn asks_about_the_project(asked: &str) -> bool {
    const ABOUT_HERE: [&str; 10] = [
        "the source",
        "the sources",
        "the code",
        "this project",
        "this repo",
        "this crate",
        "the project",
        "codebase",
        "in here",
        "from the file",
    ];
    let asked = asked.to_lowercase();
    ABOUT_HERE.iter().any(|m| asked.contains(m))
}

/// What the model is told when it answered about the project without opening it.
const NOT_LOOKED: &str = "You answered without reading anything from the project. The \
                          question is about this code, the files are listed above and you \
                          have tools to read and search them. Look first, then answer from \
                          what is actually there.";

/// Whether a name in the catalogue is something a turn could be had with.
///
/// A server that does more than text lists what it does: voices, transcribers, image pipelines, an
/// encoder or two.
/// None of them can answer a question, and offering them to somebody choosing a model is offering a
/// choice that cannot work.
///
/// Judged on the name, which is all the catalogue gives.
/// A name that says nothing either way is kept: refusing to offer a model because its name was
/// unfamiliar would be worse than listing one that turns out to be a transcriber.
pub fn can_hold_a_conversation(name: &str) -> bool {
    const NOT_A_CHAT: [&str; 14] = [
        "whisper",
        "tts",
        "voice",
        "xtts",
        "ace-step",
        "flux",
        "-image",
        "image-",
        "clip",
        "embed",
        "rerank",
        "guard",
        "moderation",
        "stable-audio",
    ];
    let name = name.to_lowercase();
    !NOT_A_CHAT.iter().any(|m| name.contains(m))
}

/// Whether the text is a failure the server wrote into the stream rather than an answer.
///
/// A server that fails part-way through generation reports it in the content, where it arrives
/// shaped exactly like what the model said: same field, same stream, no error status.
/// Nothing distinguishes it except that it is a bracketed error and nothing else.
pub fn is_backend_failure(text: &str) -> bool {
    let text = text.trim();
    text.starts_with('[') && text.to_lowercase().contains("error")
}

/// The registered tool whose name is closest to one that does not exist.
///
/// Closest by the words in the name rather than by edit distance: an invented name is a paraphrase,
/// `list_files` for `list_dir` or `run_command` for `bash`, and the letters have less to say about
/// that than the words do.
fn nearest_tool(reg: &Registry, wanted: &str) -> Option<String> {
    let parts = |s: &str| -> Vec<String> {
        s.split(['_', '-', ' '])
            .filter(|p| !p.is_empty())
            .map(str::to_lowercase)
            .collect()
    };
    let akin = |a: &str, b: &str| a.starts_with(b) || b.starts_with(a);
    let want = parts(wanted);
    let mut best: Option<((usize, usize), &String)> = None;
    for (name, _) in reg.iter() {
        let have = parts(name);
        let shared = want
            .iter()
            .filter(|w| have.iter().any(|h| akin(h, w)))
            .count();
        if shared == 0 {
            continue;
        }
        // The first word is the verb, and 2 tools that share only a noun are not the same tool:
        // read_file and list_files share "file" and do opposite things.
        let verb = usize::from(match (want.first(), have.first()) {
            (Some(w), Some(h)) => akin(h, w),
            _ => false,
        });
        let score = (shared, verb);
        if best.is_none_or(|(b, _)| score > b) {
            best = Some((score, name));
        }
    }
    best.map(|(_, name)| name.clone())
}

/// What makes 2 calls the same call: the tool, and the arguments it was given.
fn repeat_key(call: &ResolvedCall) -> String {
    format!("{}\u{0}{}", call.name, call.arguments)
}

/// A result the model has already been given, handed back with the fact said plainly.
///
/// Said rather than hidden: a model that is repeating itself has lost its place, and the only thing
/// that changes that is being told.
fn said_before(known: &ToolOutput) -> ToolOutput {
    ToolOutput {
        content: format!("{SAID_BEFORE}\n{}", known.content),
        is_error: known.is_error,
    }
}

/// How a repeated answer opens, so the loop can tell one when it sees it again.
const SAID_BEFORE: &str = "you already ran this and nothing has been written since, so \
                           the answer is the one you had:";

/// Whether an answer is a tool call the model had to write out because it was never given the
/// tools.
///
/// The shape varies with the model: `read_file('a.rs')`, `read_file src/a.rs` in a shell
/// fence, or the JSON it would have sent.
/// The test is the name itself, as a word.
/// A model that names a tool it did not call was not given it.
fn calls_a_tool_in_prose(reg: &Registry, text: &str) -> bool {
    reg.iter().any(|(name, _)| names_word(text, name))
}

/// Whether `word` appears in `text` on its own, not inside a longer identifier.
fn names_word(text: &str, word: &str) -> bool {
    let part = |c: char| c.is_alphanumeric() || c == '_';
    text.match_indices(word).any(|(i, _)| {
        let before = text[..i].chars().next_back().is_none_or(|c| !part(c));
        let after = text[i + word.len()..]
            .chars()
            .next()
            .is_none_or(|c| !part(c));
        before && after
    })
}

/// The reasoning of an answer that said nothing and called nothing, unless it was cut off.
/// A server that put the whole answer in the reasoning left the model's calls there.
fn stranded_reasoning(c: &Completion) -> Option<&str> {
    (c.text.trim().is_empty()
        && c.calls.is_empty()
        && c.finish_reason.as_deref() != Some("length")
        && !c.reasoning.trim().is_empty())
    .then_some(c.reasoning.as_str())
}

/// What the model is told when its reasoning holds a call that could not be read.
const RESTATE_CALL: &str = "Your answer was empty. The tool call you wrote is in your \
                            reasoning, where it is not read as a call. Make the call again \
                            as a tool call.";

/// One model response, with whatever the server said about how it ended.
#[derive(Default)]
struct Completion {
    text: String,
    /// Native tool-call fragments, still to be reassembled.
    calls: CallAccumulator,
    /// The model's scratchpad, kept to recover a call written there instead of in the answer.
    reasoning: String,
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
    /// Which model guesses the next read while this one thinks, and `None` for a session that does
    /// no guessing.
    /// Resolved once, from what the backend already holds: a model that would have to be loaded
    /// first is an eviction, not a guess.
    pub guess_with: Option<String>,
    /// Which model does which kind of work, when the project says.
    pub roles: crate::agent::roles::Roles,
    /// Commands to run at the edges of a tool call and a turn.
    pub hooks: crate::config::HooksSection,
    /// The window to use for a model the configuration says nothing about.
    pub context_tokens: usize,
    /// What each named model needs, when it differs from the default.
    pub models: std::collections::BTreeMap<String, crate::config::ModelSection>,
    /// The standing instructions the preamble carries, kept so a protocol change or a model change
    /// can rebuild the preamble without reading the files again.
    pub instructions: Option<String>,
    /// Where a chosen model is written down for the next session.
    /// `None` remembers nothing, which is what a test and a one-shot run both want.
    pub remember_model_in: Option<std::path::PathBuf>,
    pub grammar_enabled: bool,
    pub max_steps: usize,
    pub max_answer_tokens: usize,
    /// How to ask for tools.
    /// Changed at most once per session, when a backend turns out not to support the native
    /// protocol.
    pub protocol: ToolProtocol,
    /// Whether a refusal of native tool calls may drop the session to the envelope.
    ///
    /// False when the user pinned a protocol: asking for one and silently getting the other is
    /// worse than being told the backend cannot do what was asked.
    pub allow_fallback: bool,
    /// The protocol cell a delegated or background session reads when it starts.
    ///
    /// Which protocol works is the backend's property, so a session that finds out has to put the
    /// answer somewhere the sessions started after it will look.
    pub shared_protocol: Option<crate::agent::protocol::SharedProtocol>,
    /// Whether the turn was handed over by another session.
    /// Its question is taken as written: no file named in it is attached and the project is not
    /// searched for it.
    pub delegated: bool,
}

/// What one turn did, for the line that closes it.
///
/// A turn ends with the model's answer and nothing about the turn: how long it took, what it
/// changed, what it cost.
/// Those are the 3 things a person checks before deciding whether to look at the diff, and all 3
/// were already known and thrown away.
#[derive(Debug, Clone, Default)]
pub struct TurnTally {
    pub files: Vec<String>,
    pub steps: u32,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub started: Option<std::time::Instant>,
    /// What the backend's measurements stood at when the turn began.
    pub energy_from: crate::backend::meter::Totals,
    /// The turn's number, as the change history counts turns.
    pub turn: u32,
    /// How many times a turn-end hook kept the turn going.
    pub held: u8,
    /// How many times a proof of reconciling an agent's work was refused this turn.
    pub proofs_refused: u8,
    /// What the turn-end hooks said at the answer, once they let it end.
    pub turn_end: Option<Vec<crate::agent::hooks::Ran>>,
    /// Whether the model was asked to make again a call it left in its reasoning.
    pub restated: bool,
}

impl TurnTally {
    fn begin() -> Self {
        Self {
            started: Some(std::time::Instant::now()),
            energy_from: crate::backend::meter::ledger().totals(),
            ..Default::default()
        }
    }

    fn touched(&mut self, path: &str) {
        let path = path.to_string();
        if !self.files.contains(&path) {
            self.files.push(path);
        }
    }

    /// What the turn came to, or nothing when there is nothing worth saying.
    pub fn receipt(&self) -> Option<Receipt> {
        let took = self.started?.elapsed();
        // A turn that answered in a moment and changed nothing does not need a receipt.
        if self.steps <= 1 && self.files.is_empty() && took.as_secs_f64() < 10.0 {
            return None;
        }
        let energy = self.energy().sum();
        Some(Receipt {
            turn: self.turn,
            took,
            steps: self.steps,
            tokens_in: self.tokens_in,
            tokens_out: self.tokens_out,
            files: self.files.clone(),
            energy: energy.is_measured().then_some(energy),
        })
    }

    /// What the backend measured since the turn began, by purpose.
    pub fn energy(&self) -> crate::backend::meter::Totals {
        crate::backend::meter::ledger()
            .totals()
            .since(&self.energy_from)
    }

    /// The closing line, or nothing when there is nothing worth saying.
    pub fn describe(&self) -> Option<String> {
        self.receipt().map(|r| r.describe())
    }
}

/// What the backend measured: the latest request, this turn and the session so far.
#[derive(Debug, Clone, Copy, Default)]
pub struct Engine {
    pub latest: Option<crate::backend::meter::Latest>,
    pub turn: crate::backend::meter::Spent,
    pub session: crate::backend::meter::Spent,
}

/// What a turn came to: how long, how many steps, what it read and wrote, what it changed.
#[derive(Debug, Clone)]
pub struct Receipt {
    /// The turn's number, as the change history counts turns.
    pub turn: u32,
    pub took: std::time::Duration,
    pub steps: u32,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub files: Vec<String>,
    /// The energy the backend measured for the turn, when it measured any.
    pub energy: Option<crate::backend::meter::Spent>,
}

impl Receipt {
    /// The receipt as one line of text.
    pub fn describe(&self) -> String {
        let mut parts = vec![format!("{:.0}s", self.took.as_secs_f64())];
        if self.steps > 0 {
            parts.push(format!("{} step(s)", self.steps));
        }
        if self.tokens_in + self.tokens_out > 0 {
            parts.push(format!("{} in, {} out", self.tokens_in, self.tokens_out));
        }
        match self.files.len() {
            0 => parts.push("nothing changed".to_string()),
            n if n <= 3 => parts.push(format!("changed {}", self.files.join(", "))),
            n => parts.push(format!(
                "changed {} files: {}, …",
                n,
                self.files[..2].join(", ")
            )),
        }
        if let Some(e) = &self.energy {
            parts.push(e.describe());
        }
        parts.join("  ·  ")
    }
}

/// What the status line asks the agent for between turns.
///
/// Everything here can change under the interface while a turn runs.
/// Examples are a role switch and a backend that does not do native tool calls.
/// None of it is readable from the interface's side, because the agent owns the session.
pub struct StatusReport {
    pub model: String,
    pub window: crate::agent::session::Occupancy,
    pub protocol: ToolProtocol,
}

/// A session that has been come back to.
pub struct Resumed {
    pub id: String,
    /// What was said in it, for putting back on screen: a conversation the model can see and the
    /// person cannot is worse than no conversation at all.
    pub messages: Vec<Message>,
    /// What the model was changed to, when the session was last using another one.
    pub model: Option<String>,
    /// Whether it was planning.
    pub planning: bool,
}

/// What could be answering, and what is.
pub struct Models {
    /// The one answering now.
    pub current: String,
    /// Every model the backend serves that can hold a conversation, as it named them.
    pub all: Vec<String>,
    /// Which model does which kind of work, where that has been settled.
    pub roles: Vec<(String, String)>,
}

/// What a session has spent, so far.
///
/// The server already counts the tokens and nothing showed them: a person deciding whether to hand
/// the agent a large file has no way to know what the last one cost.
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
    /// What this turn changed and what it cost, for the line that closes it.
    pub this_turn: TurnTally,
    /// Whether the compiler has already been asked about this step.
    /// Shared rather than owned because the calls of one run are dispatched together.
    pub checked_this_step: std::sync::atomic::AtomicBool,
    /// Whether this turn has already been asked to go and look.
    /// Once: a nudge that repeats is an argument.
    pub nudged: bool,
    /// The window the history is currently sized to, so the server saying the same thing again
    /// costs nothing and saying something new is noticed.
    pub window_in_use: usize,
    /// Steps in a row where every tool call failed and nothing was written.
    pub barren: u32,
    /// Calls that failed in this turn, however they were spread across its steps.
    pub failures: u32,
    /// Answers that came from a guess rather than from the model having asked before.
    ///
    /// Served plainly: a model that is handed what it was about to ask for is not repeating itself,
    /// and telling it that it is would be a lie about its own turn.
    pub from_guess: std::collections::HashSet<String>,
    /// Read-only calls already answered in this turn, by tool and arguments.
    ///
    /// A model that has lost its place asks the same question again, and a run can hold the same
    /// call twice.
    /// The answer cannot have changed: it is cleared the moment anything writes, so what is served
    /// from here is what running it would return.
    pub repeats: std::collections::HashMap<String, ToolOutput>,
    /// The model the session was on when planning began, to be given back on leaving it.
    pub before_plan: Option<String>,
    /// What each model has managed in this project, learned from the writes that did and did not
    /// check out.
    pub history: crate::agent::history::ModelHistory,
    /// What fixed a complaint here before, so the same one does not cost the same steps twice.
    pub repairs: crate::agent::repairs::Repairs,
    /// The same backend at a priority that yields, for work nobody is waiting on.
    pub background: Option<Arc<dyn crate::backend::provider::Provider>>,
    /// The model this session was on before a failed check raised it, and how many clean steps have
    /// passed since.
    /// `None` when nothing has been raised.
    pub escalated_from: Option<String>,
    pub clean_steps: usize,
    /// Whether a model is being held ready right now, so a second request cannot queue behind the
    /// first.
    pub warming: Arc<std::sync::atomic::AtomicBool>,
    /// What this session has spent.
    pub spend: Spend,
    /// What each step asked for and what it cost.
    /// Recorded before anything routes on it, because a claim that one arrangement is faster needs
    /// the other one measured too.
    pub steps: Vec<crate::agent::steps::StepRecord>,
    /// The language servers, for checking a file the agent has just written.
    /// `None` leaves writes unchecked, which is what a test without a server wants.
    pub lsp: Option<Arc<crate::lsp::LspPool>>,
    /// Lines the user typed while a turn was running.
    /// Drained at a step boundary, which is where the history can take a user message without
    /// moving a boundary the compactor reasons about.
    pub steer: mpsc::Receiver<String>,
    /// Whoever answers.
    /// The loop knows nothing about which API it is.
    pub backend: Arc<dyn Provider>,
    pub registry: Arc<Registry>,
    pub session: Session,
    pub cfg: AgentConfig,
    pub ctx: ToolCtx,
    /// Records what the agent changes, so a turn can be shown or taken back.
    pub checkpoints: Option<Arc<Checkpoints>>,
    /// The task list the model keeps, when this session keeps one, so it outlives a resume and an
    /// elision.
    pub tasks: Option<Arc<crate::tools::plan::TaskList>>,
    /// The tool servers, so a change to what one offers reaches the next turn.
    pub mcp: crate::mcp::Clients,
    /// Which turn this is, counted from one.
    /// Checkpoints are labelled with it, which is what lets `/undo` take back a turn rather than a
    /// single tool call.
    pub turn: u32,
}

impl Agent {
    /// The preamble, which states the contract the chosen protocol actually enforces.
    ///
    /// In the native protocol the tools travel in their own field and the server states the calling
    /// convention itself, so repeating a JSON schema here would describe a shape the model is not
    /// being asked for.
    pub fn build_system_prompt(
        reg: &Registry,
        protocol: ToolProtocol,
        mode: Mode,
        instructions: Option<&str>,
    ) -> String {
        let mut prompt = Self::contract(reg, protocol);
        // Stated in the contract rather than only enforced at dispatch: a model that knows it is
        // planning writes a plan, where one that is merely refused writes the same call again.
        if mode == Mode::Plan {
            prompt.push_str(
                "\n\nYou are PLANNING. Read, search and ask, but do not change anything: \
                 every tool that writes, runs a command or reaches the network is refused \
                 while planning. Finish by calling submit_plan with the plan, its steps each \
                 naming what you would run or change. The user approves it before anything \
                 changes.",
            );
        }
        // Last, and named as the project's own: a model that has read the contract first is less
        // likely to take a project's wording for the calling convention.
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

    /// What the model is told about calling tools, before anything about this turn.
    pub(crate) fn contract(reg: &Registry, protocol: ToolProtocol) -> String {
        match protocol {
            ToolProtocol::Native => format!(
                "You are a local-first code agent driving a TUI on the user's machine.\n\
                 Call the tools you have been given to inspect and change the project.\n\
                 Stop calling tools when the task is finished or when you only need to talk.\n\
                 {NEVER_ASK}\n\n\
                 {}",
                tool_manifest(reg)
            ),
            ToolProtocol::Envelope => format!(
                "You are a local-first code agent driving a TUI on the user's machine.\n\
                 Respond with a single JSON object matching the schema:\n\
                   {{ \"assistant_text\": \"<prose to user>\", \"tool_calls\": [ {{ \"name\": <tool>, \"arguments\": {{...}} }} ] }}\n\
                 Emit zero tool_calls when the task is finished or when you only need to talk.\n\
                 Tool results will be returned to you as role=tool messages on the next turn.\n\
                 {NEVER_ASK}\n\n\
                 {}",
                tool_manifest(reg)
            ),
        }
    }

    /// Swap protocol and rewrite the preamble to match.
    ///
    /// The system message states the contract, so changing how tools are asked for without changing
    /// it would leave the model reading instructions for the other one.
    fn fall_back_to_envelope(&mut self) {
        self.cfg.protocol = ToolProtocol::Envelope;
        if let Some(shared) = &self.cfg.shared_protocol {
            shared.set(ToolProtocol::Envelope);
        }
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
        lines.push("past 100% the oldest steps are elided; /compact does it now".to_string());
        lines.join("\n")
    }

    /// Plan or act, from now on.
    fn set_mode(&mut self, mode: Mode) -> String {
        if self.cfg.mode == mode {
            return format!("already in {} mode", mode.label());
        }
        self.cfg.mode = mode;
        self.ctx.planning = mode == Mode::Plan;
        if let Some(t) = self.session.transcript() {
            t.append(crate::agent::transcript::Record::Mode {
                planning: mode == Mode::Plan,
            });
        }
        self.rebuild_preamble();
        // Planning is where reasoning is worth paying for and acting is where being over quickly
        // is, so the mode carries its model with it when the project named one.
        // Said out loud, because a model that changed without anyone announcing it would make the
        // next answer's character unexplainable.
        let role = match mode {
            Mode::Plan => crate::agent::roles::Role::Plan,
            Mode::Act => crate::agent::roles::Role::Chat,
        };
        // Planning is long and ends in acting, so the model acting will use is loaded while the
        // plan is being written.
        // By the time /act is typed it is resident.
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
        // Leaving planning gives back the model planning was entered from, whether or not a chat
        // role names one: a mode that borrows a model has to return it, or the session quietly
        // finishes the day on the model it planned with.
        let target = match mode {
            Mode::Act => self
                .cfg
                .roles
                .get(role)
                .map(str::to_string)
                .or_else(|| self.before_plan.take()),
            Mode::Plan => {
                let plan_model = self.cfg.roles.get(role).map(str::to_string);
                if plan_model.is_some() {
                    self.before_plan = Some(self.cfg.model.clone());
                }
                plan_model
            }
        };
        let switched = match target {
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
    /// Asked for while a tool runs or while a plan is being written, the load is spent inside time
    /// that passes anyway.
    /// The switch that follows then costs nothing.
    ///
    /// Fire and forget, and at most one at a time.
    /// It never delays a turn, fails one, or queues up behind itself.
    /// The server decides whether it can hold 2 models at once.
    /// If it cannot, the next use of the current model reloads it.
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

    /// Shrink the history to the window the server says it actually served.
    ///
    /// The configured figure is what this session asked for.
    /// The served figure is what it got, and it can be smaller.
    /// The larger of the 2 must not be believed.
    /// A prompt sent too big is cut from the front by the server, which drops the standing
    /// instructions.
    ///
    /// A ceiling, never a raise.
    /// A server serving a larger window because somebody else asked for one is not an invitation to
    /// fill it.
    async fn adopt_served_window(&mut self, events: &mpsc::Sender<AgentEvent>) {
        let Some(served) = self.backend.served_window() else {
            return;
        };
        let asked = self
            .cfg
            .models
            .get(&self.cfg.model)
            .and_then(|m| m.context_tokens)
            .unwrap_or(self.cfg.context_tokens);
        self.adopt_window(asked.min(served), events).await;
    }

    /// Size the history to `window`, and say so when it moves.
    ///
    /// One place sizes the history, whichever way the window was learned.
    /// A backend either answers the question, or cuts a prompt and is caught at it.
    async fn adopt_window(&mut self, window: usize, events: &mpsc::Sender<AgentEvent>) {
        if window == 0 || window == self.window_in_use {
            return;
        }
        let previous = self.window_in_use;
        self.window_in_use = window;
        let reserve = crate::agent::session::answer_reserve(
            window,
            self.cfg
                .models
                .get(&self.cfg.model)
                .and_then(|m| m.max_answer_tokens)
                .unwrap_or(self.cfg.max_answer_tokens),
        );
        self.session.set_window(window, reserve);
        // What a tool result may contribute is a share of the same window, so it moves with it: a
        // history resized upward that still reads files in 2-kilobyte pieces has not gained
        // anything worth having.
        self.ctx.max_output_chars = crate::agent::session::tool_output_chars(window, reserve);
        if previous != 0 {
            let _ = events
                .send(AgentEvent::Notice(format!(
                    "the history is sized to {window} tokens for {}",
                    self.cfg.model
                )))
                .await;
        }
    }

    /// Take the window a model needs and forget what the last one taught, which every model change
    /// has to do whether a person asked for it or a role did.
    fn forget_model_calibration(&mut self, model: &str) {
        let per_model = self.cfg.models.get(model);
        let context = per_model
            .and_then(|m| m.context_tokens)
            .unwrap_or(self.cfg.context_tokens);
        let reserve = crate::agent::session::answer_reserve(
            context,
            per_model
                .and_then(|m| m.max_answer_tokens)
                .unwrap_or(self.cfg.max_answer_tokens),
        );
        self.session.set_window(context, reserve);
        self.ctx.max_output_chars = crate::agent::session::tool_output_chars(context, reserve);
        // The next answer settles what this model is really served with.
        self.window_in_use = context;
    }

    /// Answer one control request.
    /// Between turns, which is where the session can change under the model without a request being
    /// half-built against the old one.
    pub async fn control(&mut self, request: Control, events: &mpsc::Sender<AgentEvent>) {
        match request {
            Control::Model { name, reply } => {
                let answer = self.switch_model(name).await;
                let _ = reply.send(answer);
            }
            Control::Models { reply } => {
                let _ = reply.send(self.models_on_offer().await);
            }
            Control::SeeWindow(served) => {
                let asked = self
                    .cfg
                    .models
                    .get(&self.cfg.model)
                    .and_then(|m| m.context_tokens)
                    .unwrap_or(self.cfg.context_tokens);
                self.adopt_window(asked.min(served), events).await;
            }
            Control::Resume { id, reply } => {
                let _ = reply.send(self.resume_session(&id).await);
            }
            Control::Clear { reply } => {
                let dropped = self.session.clear();
                if let Some(list) = &self.tasks {
                    list.set(Vec::new()).await;
                }
                tracing::info!(dropped, "conversation cleared");
                let _ = reply.send(match dropped {
                    0 => "nothing had been said yet".to_string(),
                    n => format!("started again; {n} message(s) dropped"),
                });
            }
            Control::RestartMcp {
                server,
                spec,
                reply,
            } => {
                let old = crate::mcp::snapshot(&self.mcp)
                    .into_iter()
                    .find(|c| c.server == server);
                let mut registry = (*self.registry).clone();
                let started = match (old, spec) {
                    (Some(old), _) => {
                        Some(crate::mcp::restart(&old, &self.mcp, &mut registry).await)
                    }
                    (None, Some(spec)) => Some(
                        crate::mcp::add(&server, &spec, &self.ctx.root, &self.mcp, &mut registry)
                            .await,
                    ),
                    (None, None) => None,
                };
                let said = match started {
                    None => format!("no server called {server}"),
                    Some(Ok(said)) => {
                        self.registry = Arc::new(registry);
                        self.rebuild_preamble();
                        said
                    }
                    Some(Err(e)) => format!("{server} did not start: {e:#}"),
                };
                let _ = reply.send(said);
            }
            Control::Note(text) => {
                // As something the person said, because that is what it is: they ran the command
                // and they are handing over what it printed.
                // A role of its own would be a role every backend renders differently.
                self.session.push(Message::user(text));
            }
            Control::Protocol(protocol) => {
                // Only where the protocol was left open.
                // A session told which protocol to speak keeps speaking it, and is told when the
                // backend disagrees.
                if self.cfg.allow_fallback && self.cfg.protocol != protocol {
                    self.cfg.protocol = protocol;
                    if let Some(shared) = &self.cfg.shared_protocol {
                        shared.set(protocol);
                    }
                    self.rebuild_preamble();
                }
            }
            Control::Retry { model, reply } => {
                let answer = match self.session.rewind_to_last_ask() {
                    None => Err("nothing has been asked yet".to_string()),
                    Some(asked) => {
                        if let Some(name) = model {
                            let said = self.switch_model(Some(name)).await;
                            tracing::info!(%said, "retry switched model");
                        }
                        Ok(asked)
                    }
                };
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
                self.carry_tasks().await;
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
            Control::Status { reply } => {
                let _ = reply.send(StatusReport {
                    model: self.cfg.model.clone(),
                    window: self.session.measure(),
                    protocol: self.cfg.protocol,
                });
            }
            Control::Why { reply } => {
                let _ = reply.send(self.explain_choice());
            }
            Control::Spend { reply } => {
                let _ = reply.send(self.spend_page());
            }
        }
    }

    /// What the session spent, as a page: the totals, the energy by what it was for when the
    /// backend measures it, and the last steps, most recent first.
    fn spend_page(&self) -> String {
        let s = &self.spend;
        if s.steps == 0 {
            return s.describe();
        }
        let rate = match s.model_ms {
            0 => 0.0,
            ms => s.completion_tokens as f64 * 1000.0 / ms as f64,
        };
        let mut out = format!(
            "## This session\n\n\
             - **{}** steps\n\
             - **{}** tokens read, **{}** written, at **{rate:.0}** tokens a second\n\
             - **{:.1}s** in the model, **{:.1}s** in tools\n",
            s.steps,
            s.prompt_tokens,
            s.completion_tokens,
            s.model_ms as f64 / 1000.0,
            s.tool_ms as f64 / 1000.0
        );
        let by = crate::backend::meter::ledger().totals();
        if by.sum().is_measured() {
            out.push_str("\n## Energy\n\n| What for | Energy | Requests |\n|---|---|---|\n");
            for (purpose, spent) in by.by_purpose() {
                out.push_str(&format!(
                    "| {} | {} | {} |\n",
                    purpose.label(),
                    spent.describe(),
                    spent.requests
                ));
            }
            out.push_str(&format!(
                "| **all of it** | **{}** | {} |\n",
                by.sum().describe(),
                by.sum().requests
            ));
        }
        if !self.steps.is_empty() {
            out.push_str(
                "\n## Last steps\n\n\
                 | Turn | Step | Tools | Model | Read | Written | Check |\n\
                 |---|---|---|---|---|---|---|\n",
            );
            for r in self.steps.iter().rev().take(8) {
                let tools = match r.after.tools.is_empty() {
                    true => "answered".to_string(),
                    false => r.after.tools.join(", "),
                };
                let check = match r.after.check_passed {
                    Some(true) => "ok",
                    Some(false) => "failed",
                    None => "",
                };
                out.push_str(&format!(
                    "| {} | {} | {tools} | {:.1}s | {} | {} | {check} |\n",
                    r.turn,
                    r.before.step,
                    r.after.model_ms as f64 / 1000.0,
                    r.after.prompt_tokens,
                    r.after.completion_tokens
                ));
            }
        }
        out
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
        lines.push(format!(
            "what has been fixed here before:\n{}",
            self.repairs.describe()
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

    /// Come back to a session recorded earlier.
    ///
    /// The record is appended to rather than replaced, so a session resumed 3 times is one story
    /// rather than 4, and the model it was last using comes back with it.
    async fn resume_session(&mut self, id: &str) -> Result<Resumed, String> {
        let dir = crate::paths::sessions_dir()
            .ok_or_else(|| "no home directory, so no sessions to come back to".to_string())?;
        let path = dir.join(format!("{id}.jsonl"));
        if !path.exists() {
            return Err(format!("no session recorded as {id}"));
        }
        let replayed =
            crate::agent::transcript::replay(&path).map_err(|e| format!("{id}: {e:#}"))?;
        let (transcript, _writer) = crate::agent::transcript::Transcript::create(path)
            .map_err(|e| format!("{id}: {e:#}"))?;
        let messages = replayed.messages.clone();
        self.session
            .adopt(transcript, replayed.id.clone(), replayed.messages);
        if let Some(list) = &self.tasks {
            list.set(replayed.tasks.clone()).await;
        }
        let mode = match replayed.planning {
            true => Mode::Plan,
            false => Mode::Act,
        };
        if mode != self.cfg.mode {
            self.set_mode(mode);
        }
        let model = match replayed.model.is_empty() || replayed.model == self.cfg.model {
            true => None,
            false => {
                self.switch_model(Some(replayed.model.clone())).await;
                Some(replayed.model)
            }
        };
        tracing::info!(session = %replayed.id, messages = messages.len(), "session resumed");
        Ok(Resumed {
            id: replayed.id,
            messages,
            model,
            planning: replayed.planning,
        })
    }

    /// What could be answering, for a list to be chosen from.
    ///
    /// Only what can hold a conversation: a backend that also serves speech and image models offers
    /// them here too, and none of them can take a turn.
    async fn models_on_offer(&self) -> Result<Models, String> {
        let served = self
            .backend
            .models()
            .await
            .map_err(|e| format!("the backend does not say what it serves ({e:#})"))?;
        let all: Vec<String> = served
            .into_iter()
            .filter(|m| can_hold_a_conversation(m))
            .collect();
        if all.is_empty() {
            return Err("the backend serves no model that can hold a conversation".to_string());
        }
        Ok(Models {
            current: self.cfg.model.clone(),
            all,
            roles: self
                .cfg
                .roles
                .named()
                .into_iter()
                .map(|(role, model)| (role.to_string(), model.to_string()))
                .collect(),
        })
    }

    /// Switch to `name`, or describe what is on offer when there is none.
    ///
    /// A backend that cannot list its models is not a backend that cannot serve one, so a name is
    /// taken on trust when the listing fails.
    /// A name the backend does list and does not serve is refused, because the alternative is a
    /// session that fails on every turn with a message from the server.
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
                        .filter(|m| can_hold_a_conversation(m))
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
        // A window belongs to a model, and so does the tokenizer the budget was calibrated against.
        // Carrying either over carries a measurement of the model that just left.
        self.forget_model_calibration(&name);
        let context = self.session.budget_chars();
        if let Some(t) = self.session.transcript() {
            t.append(crate::agent::transcript::Record::Model { name: name.clone() });
        }
        tracing::info!(from = %previous, to = %name, context, "model changed");
        // Remembered for the next session.
        // Failing to write is worth saying and is not worth refusing the switch over.
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

    /// Run one user turn end-to-end.
    /// Loops model, tools, model until the model emits zero tool_calls, `max_steps` is reached, or
    /// `cancel` fires.
    ///
    /// `cancel` is created by the UI before the input is handed over.
    /// The side that reacts to a keystroke thus already holds the handle it needs.
    /// It is stored on the tool context for the length of the turn.
    /// A tool waiting on a subprocess learns from it that the user has stopped waiting.
    ///
    /// The model for one kind of work: what the project named for that role, else the session's own
    /// model.
    /// Nothing here decides anything.
    /// It answers what is available to whoever is deciding.
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

    /// Run one turn to the end and hand back everything it said.
    ///
    /// The events channel is bounded.
    /// A caller that reads it only once the turn has returned stops the turn at the 60-fourth event
    /// and waits there forever, so a sub-session that reports its own progress has to be drained
    /// while it runs.
    pub async fn collect_turn(
        &mut self,
        user_input: String,
        cancel: CancellationToken,
    ) -> Result<Vec<AgentEvent>> {
        let (tx, mut rx) = mpsc::channel(64);
        let drain = tokio::spawn(async move {
            let mut events = Vec::new();
            while let Some(event) = rx.recv().await {
                events.push(event);
            }
            events
        });
        let outcome = self.run_user_turn(user_input, cancel, tx).await;
        let events = drain.await.unwrap_or_default();
        outcome.map(|()| events)
    }

    pub async fn run_user_turn(
        &mut self,
        user_input: String,
        cancel: CancellationToken,
        events: mpsc::Sender<AgentEvent>,
    ) -> Result<()> {
        // What this project taught is applied once, at the start.
        // The ladder reacts to a change that has already failed to compile.
        // The memory of those steps lets a turn start where the last one ended.
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

        self.this_turn = TurnTally::begin();
        self.nudged = false;
        let asked = user_input.clone();
        if let Ok(mut said) = self.ctx.asked.lock() {
            said.push(user_input.chars().take(1000).collect());
        }
        self.repeats.clear();
        self.from_guess.clear();
        self.barren = 0;
        self.failures = 0;

        // The cap on what a policy may grant is about one piece of work, so it starts again with
        // each turn rather than running out over a session.
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
        self.take_changed_tools(&events).await;
        self.turn += 1;
        self.this_turn.turn = self.turn;
        tracing::info!(
            session = %self.session.id,
            chars = user_input.len(),
            "turn started"
        );
        // A file the person named travels with the question rather than being fetched after it:
        // they have already decided it is relevant, and on a local model the round trip that would
        // fetch it costs seconds.
        // What the hooks make of the question: refused, or sent with what they add.
        // The session's own hooks go with its first question.
        let mut hooks_said = Vec::new();
        if !self.cfg.delegated {
            use crate::agent::hooks::{run_all, said, Event};
            if self.turn == 1 {
                let rans = run_all(
                    &self.cfg.hooks.session_start,
                    Event::SessionStart,
                    &self.ctx.root,
                    None,
                    self.hook_input(Event::SessionStart, serde_json::json!({})),
                )
                .await;
                hooks_said.extend(said(Event::SessionStart, &rans));
            }
            let rans = run_all(
                &self.cfg.hooks.prompt,
                Event::Prompt,
                &self.ctx.root,
                None,
                self.hook_input(Event::Prompt, serde_json::json!({"prompt": user_input})),
            )
            .await;
            if let Some(refusal) = rans.iter().find(|r| r.blocked) {
                let _ = events
                    .send(AgentEvent::Notice(format!(
                        "a prompt hook refused the question:\n{}",
                        refusal.output
                    )))
                    .await;
                let _ = events.send(AgentEvent::TurnDone).await;
                return Ok(());
            }
            hooks_said.extend(said(Event::Prompt, &rans));
        }
        // A line that opens on an agent's name hands the rest to that agent.
        let user_input = match self.agent_named(&user_input) {
            Some((agent, rest)) => format!(
                "Hand this to the agent {agent} with delegate, then report what it said: {rest}"
            ),
            None => user_input,
        };
        let expanded = match self.cfg.delegated {
            true => crate::agent::mentions::Expanded {
                text: user_input.clone(),
                attached: Vec::new(),
            },
            false => crate::agent::mentions::expand(
                &user_input,
                &self.ctx.root,
                self.ctx.max_output_chars,
            ),
        };
        if !expanded.attached.is_empty() {
            let _ = events
                .send(AgentEvent::Notice(format!(
                    "sent with the question: {}",
                    expanded.attached.join(", ")
                )))
                .await;
        }
        // And what the project itself has to say about the question.
        // The search for the relevant files is done here, so the turn starts from what is there.
        //
        // Not when files were named.
        // The person has already said what is relevant.
        let mut text = expanded.text;
        for note in hooks_said {
            text.push_str(&format!("\n\n{note}"));
        }
        if !self.cfg.delegated && expanded.attached.is_empty() && asks_about_the_project(&asked) {
            if let Some(found) =
                crate::agent::retrieve::passages(&asked, &self.ctx.root, self.ctx.max_output_chars)
            {
                let _ = events
                    .send(AgentEvent::Notice(format!(
                        "searched the project for it: {}",
                        found.files.join(", ")
                    )))
                    .await;
                text = format!("{text}\n\n{}", found.text);
            }
        }
        self.session.push(Message::user(text));

        let turn_schema = build_turn_schema(&self.registry);
        let validator = Validator::new(&turn_schema).map_err(|e| anyhow::anyhow!("{e}"))?;

        for step in 0..self.cfg.max_steps {
            // One build per step: a step that writes 3 files is one change, and asking 3 times
            // costs 3 builds to learn the same thing.
            self.checked_this_step
                .store(false, std::sync::atomic::Ordering::Relaxed);
            if cancel.is_cancelled() {
                self.abandon(&events, "interrupted").await;
                return Ok(());
            }
            // Before every call, not once per turn: a single tool result can be larger than
            // everything said so far, so the history can outgrow the window in the middle of a turn
            // as easily as between 2.
            // Anything said while the previous step ran joins the conversation here, before the
            // request is built, so the model reads it on this step rather than after the turn it
            // was meant to change.
            while let Ok(said) = self.steer.try_recv() {
                self.session.push(Message::user(said.clone()));
                let _ = events.send(AgentEvent::Steered(said)).await;
            }
            let _ = events.send(AgentEvent::Phase(Phase::Preparing)).await;
            self.carry_tasks().await;
            let dropped = self.session.compact();
            if dropped > 0 && !self.cfg.hooks.compact.is_empty() {
                use crate::agent::hooks::{run_all, said, Event};
                let rans = run_all(
                    &self.cfg.hooks.compact,
                    Event::Compact,
                    &self.ctx.root,
                    None,
                    self.hook_input(Event::Compact, serde_json::json!({"dropped": dropped})),
                )
                .await;
                if let Some(text) = said(Event::Compact, &rans) {
                    let _ = events.send(AgentEvent::Notice(text)).await;
                }
            }
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
            // Opened before the request goes out.
            // It holds what a decision about which model to ask could have used.
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
            self.adopt_served_window(&events).await;
            // Sent every step rather than at the end of the turn: a window filling up is something
            // to see while there is still time to compact, not afterwards.
            let _ = events
                .send(AgentEvent::Window(self.session.measure()))
                .await;
            let _ = events.send(AgentEvent::Phase(Phase::Waiting)).await;
            let asked_at = std::time::Instant::now();
            // A small model guesses which file the answer is going to ask for while the answer is
            // written.
            // A read has no effect to undo when the guess is wrong.
            //
            // Abandoned the moment the answer arrives.
            // A guess that has not landed first is never something the turn waits on.
            let guessing = self.start_guess(&cancel);
            let mut attempt = self.stream_one(&turn_schema, &cancel, &events).await;
            if let Some((handle, stop)) = guessing {
                stop.cancel();
                if let Ok(Some((key, out))) = handle.await {
                    if self.repeats.insert(key.clone(), out).is_none() {
                        self.from_guess.insert(key);
                    }
                }
            }
            record.after.model_ms = asked_at.elapsed().as_millis() as u64;
            self.spend.model_ms += record.after.model_ms;
            if let Some(rate) = step_rate(&attempt, record.after.model_ms) {
                let _ = events.send(AgentEvent::Rate(rate)).await;
            }
            // What the backend measured, when it measures anything: the request just made, this
            // turn and the session.
            let ledger = crate::backend::meter::ledger();
            if let Some(latest) = ledger.latest() {
                let _ = events
                    .send(AgentEvent::Engine(Engine {
                        latest: Some(latest),
                        turn: self.this_turn.energy().sum(),
                        session: ledger.totals().sum(),
                    }))
                    .await;
            }

            // A backend that will not take native tool calls says so by refusing the request.
            // Nothing has been said to the user at that point.
            // The same step can be run again under the protocol that does not need the server's
            // cooperation.
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

            // A backend can also take the tool definitions and drop them.
            // Nothing fails: the model is simply never told it has tools, and writes the call it
            // wanted to make into its answer.
            // The tell is an answer that names a tool the model was given no way to call.
            if self.cfg.allow_fallback
                && self.cfg.protocol == ToolProtocol::Native
                && step == 0
                && matches!(&attempt, Ok(c) if c.calls.is_empty()
                    && calls_a_tool_in_prose(&self.registry, &c.text))
            {
                tracing::warn!("the backend ignored the tool definitions; using the envelope");
                self.fall_back_to_envelope();
                let _ = events
                    .send(AgentEvent::Notice(
                        "the backend accepted the tool definitions and ignored them; \
                         asking again with the JSON envelope"
                            .into(),
                    ))
                    .await;
                let _ = events.send(AgentEvent::Phase(Phase::Waiting)).await;
                let asked_at = std::time::Instant::now();
                attempt = self.stream_one(&turn_schema, &cancel, &events).await;
                record.after.model_ms += asked_at.elapsed().as_millis() as u64;
                self.spend.model_ms += asked_at.elapsed().as_millis() as u64;
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

            // The server's count is the only tokenizer this client can reach, so it is what the
            // chars-per-token estimate is corrected against.
            self.spend.steps += 1;
            self.this_turn.steps += 1;
            record.after.finish_reason = completion.finish_reason.clone();
            if let Some(usage) = completion.usage {
                record.after.prompt_tokens = usage.prompt_tokens;
                record.after.completion_tokens = usage.completion_tokens;
                record.after.loken = usage.loken;
                self.this_turn.tokens_in += usage.prompt_tokens as u64;
                self.this_turn.tokens_out += usage.completion_tokens as u64;
                self.spend.prompt_tokens += usage.prompt_tokens as u64;
                self.spend.completion_tokens += usage.completion_tokens as u64;
                match self.session.observe(sent_chars, usage.prompt_tokens) {
                    crate::agent::session::Observed::Quiet => {}
                    crate::agent::session::Observed::Budget(note) => {
                        let _ = events.send(AgentEvent::Notice(note)).await;
                    }
                    // The backend cut the prompt, and what it read is the window it serves.
                    // Saying so and carrying on would send the next step the same way: the history
                    // is sized to what was learned, which is the same thing done when a backend
                    // answers the question directly.
                    crate::agent::session::Observed::Cut(read) => {
                        self.adopt_window(read, &events).await;
                        let _ = events
                            .send(AgentEvent::Notice(format!(
                                "the backend read only {read} tokens of what was sent, so it \
                                 is cutting the prompt: the history is now sized to that"
                            )))
                            .await;
                    }
                }
            }
            // A response stopped by the token limit is a truncated JSON object.
            // Without this it goes into the repair path looking like a model that cannot write
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
                // A server that fails part-way through generation puts the failure in the content,
                // where it reads as the model's answer.
                // Said plainly, because a turn that ends on one has not been answered.
                if is_backend_failure(&turn.text) {
                    let _ = events
                        .send(AgentEvent::Notice(
                            "that answer is the backend reporting a failure, not the \
                             model: ask again."
                                .into(),
                        ))
                        .await;
                }
            }

            self.record_assistant_turn(&turn)?;

            if turn.calls.is_empty() {
                // A call left in the reasoning that could not be read is asked for again, once.
                if turn.stranded && !self.this_turn.restated {
                    self.this_turn.restated = true;
                    let _ = events
                        .send(AgentEvent::Notice(
                            "the model wrote a tool call in its reasoning that could not be \
                             read; asking it to make the call"
                                .into(),
                        ))
                        .await;
                    self.session.push(Message::user(RESTATE_CALL));
                    self.close_step(record);
                    continue;
                }
                // A question about the project, answered without opening the project.
                //
                // The loop ends a turn the moment the model stops calling tools, which takes the
                // model's word for it that the question has been engaged with.
                // A model that answers about a codebase from its own memory, or from the one file
                // it happened to read, is answering a different question, and nothing here noticed.
                // It is told once, and only once: a nudge that repeats is an argument.
                if !self.nudged && asks_about_the_project(&asked) && self.this_turn.steps <= 1 {
                    self.nudged = true;
                    let _ = events
                        .send(AgentEvent::Notice(
                            "answered without reading anything; asking it to look".into(),
                        ))
                        .await;
                    self.session.push(Message::user(NOT_LOOKED));
                    self.close_step(record);
                    continue;
                }
                // Work an agent did in its own copy is brought in before the turn ends.
                if let Some(waiting) = self.held_by_work() {
                    let _ = events
                        .send(AgentEvent::Notice(format!("the turn goes on: {waiting}")))
                        .await;
                    self.session.push(Message::user(format!(
                        "Before the turn ends: {waiting}. Read its patch, then call take_work."
                    )));
                    self.close_step(record);
                    continue;
                }
                // A turn-end hook that blocks hands its reason back, and the turn goes on.
                if let Some(reason) = self.held_by_turn_end(&turn.text).await {
                    let _ = events
                        .send(AgentEvent::Notice(format!(
                            "a turn-end hook keeps the turn going:\n{reason}"
                        )))
                        .await;
                    self.session.push(Message::user(format!(
                        "A turn-end hook does not let the turn end yet:\n{reason}"
                    )));
                    self.close_step(record);
                    continue;
                }
                self.close_step(record);
                self.end_turn(&events, Some(crate::sound::Outcome::Success))
                    .await;
                return Ok(());
            }
            record.after.tools = turn.calls.iter().map(|c| c.name.clone()).collect();

            // Whether this step got anywhere.
            // A call that failed learned nothing, and a call already answered this turn learned
            // nothing either: the memo saves the work but not the round trip, and a model asking
            // the same question every step will do it until the step limit.
            let mut called = 0usize;
            let mut fruitless = 0usize;
            // Read-only calls in a run go together.
            // A turn that reads 3 files would otherwise wait for 3 round trips, one after another.
            // None of the calls can see what the others did.
            //
            // A mutating call ends the run.
            // What it changes is what the next call would read.
            // A checkpoint has to sit between them to mean anything.
            for run in runs(&self.registry, turn.calls) {
                if cancel.is_cancelled() {
                    // Every call the model just made needs a result, even the ones that will never
                    // run: the assistant turn listing them is already in the history, and a call
                    // with no answer behind it describes an action whose outcome the model cannot
                    // see.
                    for call in run {
                        self.session
                            .push(Message::tool("[error] interrupted by user", call.id));
                    }
                    continue;
                }

                // Announced before any of them starts, in the order the model asked, so the screen
                // reads as a list of what is happening rather than as whatever finished first.
                for call in &run {
                    let args_pretty =
                        serde_json::to_string_pretty(&call.arguments).unwrap_or_default();
                    let _ = events
                        .send(AgentEvent::ToolCall {
                            id: call.id.clone(),
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
                // A test suite or a build runs for a while inside a turn.
                // The model the next mode will want can be read in behind it.
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
                // A second pair of eyes on anything that changes something, when the project has
                // named a model for it.
                // Asked before the write rather than after, because after is a file to put back.
                let mut objections: std::collections::HashMap<String, String> = Default::default();
                if self
                    .cfg
                    .roles
                    .get(crate::agent::roles::Role::Review)
                    .is_some()
                {
                    for call in &run {
                        let mutating = self
                            .registry
                            .get(&call.name)
                            .map(|t| t.mutates())
                            .unwrap_or(false);
                        if !mutating {
                            continue;
                        }
                        if let Some(said) = self.second_opinion(call, &turn.text, &cancel).await {
                            let _ = events
                                .send(AgentEvent::Notice(format!(
                                    "second opinion on {}: {said}",
                                    call.name
                                )))
                                .await;
                            objections.insert(call.id.clone(), said);
                        }
                    }
                }
                // What actually runs.
                // A read-only call this turn already made, with nothing written since, has an
                // answer: running it again spends a round trip to be told the same thing, and a
                // model that is going in circles does exactly that.
                let mut fresh: Vec<&ResolvedCall> = Vec::new();
                let mut plan: Vec<Result<usize, (ToolOutput, Origin)>> =
                    Vec::with_capacity(run.len());
                for call in &run {
                    if objections.contains_key(&call.id) {
                        plan.push(Err((
                            ToolOutput::err(format!(
                                "not run: a second reading of this change says {}. Fix that \
                                 or say why it is right.",
                                objections[&call.id]
                            )),
                            Origin::Withheld,
                        )));
                        continue;
                    }
                    let key = repeat_key(call);
                    if let Some(known) = self.repeats.get(&key) {
                        // Handed what it was about to ask for, or told it is going in circles: the
                        // same answer, and only one of them is true.
                        plan.push(Err(match self.from_guess.remove(&key) {
                            true => (known.clone(), Origin::Guessed),
                            false => (said_before(known), Origin::Repeated),
                        }));
                    } else if let Some(i) = fresh.iter().position(|c| repeat_key(c) == key) {
                        plan.push(Ok(i));
                    } else {
                        fresh.push(call);
                        plan.push(Ok(fresh.len() - 1));
                    }
                }
                let ran_at = std::time::Instant::now();
                let ran = futures::future::join_all(fresh.iter().map(|call| async {
                    let started = std::time::Instant::now();
                    let out = self
                        .dispatch_tool(&call.id, &call.name, call.arguments.clone())
                        .await;
                    (out, started.elapsed())
                }))
                .await;
                let mut served = std::collections::HashSet::new();
                let results: Vec<(ToolOutput, std::time::Duration, Origin)> = plan
                    .into_iter()
                    .map(|slot| match slot {
                        Ok(i) if served.insert(i) => (ran[i].0.clone(), ran[i].1, Origin::Ran),
                        Ok(i) => (said_before(&ran[i].0), Default::default(), Origin::Repeated),
                        Err((known, origin)) => (known, Default::default(), origin),
                    })
                    .collect();

                record.after.tool_ms += ran_at.elapsed().as_millis() as u64;
                self.spend.tool_ms += ran_at.elapsed().as_millis() as u64;
                for (call, (result, took, origin)) in run.into_iter().zip(results) {
                    called += 1;
                    let repeated = result.content.starts_with(SAID_BEFORE);
                    fruitless += usize::from(result.is_error || repeated);
                    self.failures += u32::from(result.is_error);
                    let key = repeat_key(&call);
                    let _ = events
                        .send(AgentEvent::ToolResult {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            output: result.clone(),
                            took,
                            origin,
                        })
                        .await;
                    let prefix = if result.is_error { "[error] " } else { "" };
                    self.session.push(Message::tool(
                        format!("{prefix}{}", result.content),
                        call.id,
                    ));

                    // After the call rather than before it: the state to return to is the one that
                    // preceded this turn, and that was recorded when the turn before it ended.
                    // Recording here also means a tool that changed nothing leaves nothing behind.
                    let mutating = self
                        .registry
                        .get(&call.name)
                        .map(|t| t.mutates())
                        .unwrap_or(false);
                    if !mutating {
                        // Only a read is worth remembering, and only until something writes: after
                        // that the answer it would give is a new answer.
                        self.repeats.insert(key, result.clone());
                    } else if !result.is_error {
                        self.repeats.clear();
                    }
                    let handed_on = !mutating
                        && !result.is_error
                        && self
                            .registry
                            .get(&call.name)
                            .is_some_and(|t| t.may_change());
                    // The list as it now stands, so a resumed session comes back to it.
                    if matches!(call.name.as_str(), "update_plan" | "submit_plan")
                        && !result.is_error
                    {
                        if let (Some(t), Some(list)) = (self.session.transcript(), &self.tasks) {
                            t.append(crate::agent::transcript::Record::Tasks {
                                tasks: list.get().await,
                            });
                        }
                    }
                    // An approved plan ends planning, and the turn carries on acting on it.
                    if call.name == "submit_plan"
                        && !result.is_error
                        && self.cfg.mode == Mode::Plan
                        && result.content.starts_with(crate::tools::plan::APPROVED)
                    {
                        let said = self.set_mode(Mode::Act);
                        let _ = events.send(AgentEvent::Mode(Mode::Act)).await;
                        let _ = events.send(AgentEvent::Notice(said)).await;
                    }
                    if handed_on {
                        for path in changed_paths(&result.content) {
                            self.this_turn.touched(&path);
                        }
                        self.repeats.clear();
                        self.record_change(&call.name, &events).await;
                    }
                    // A proof refused twice in a turn is a failed check like a write that did not
                    // compile: the stronger model is asked to do the reconciling.
                    if call.name == "take_work"
                        && result.is_error
                        && result.content.starts_with(crate::tools::worktree::REFUSED)
                    {
                        self.this_turn.proofs_refused += 1;
                        if self.this_turn.proofs_refused >= Self::PROOFS_REFUSED_AT_MOST {
                            record.after.check_passed = Some(false);
                        }
                    }
                    if mutating && !result.is_error {
                        for path in changed_paths(&result.content) {
                            self.this_turn.touched(&path);
                        }
                        record.after.wrote = true;
                        // The objective signal of whether the model needs a higher rung.
                        // Passed or failed only when a check could tell.
                        record.after.check_passed = match crate::lsp::check::read(&result.content) {
                            Some((crate::lsp::check::Outcome::Failed, _)) => Some(false),
                            Some((crate::lsp::check::Outcome::Passed, _)) => Some(true),
                            _ => None,
                        };
                        // What fixed this before, and what this is fixing now.
                        // A complaint is held until a change makes it go away, and the pair is what
                        // the next occurrence arrives with.
                        match result.content.split_once(CHECK_FAILED) {
                            Some((_, said)) => self.repairs.failed(said),
                            None if record.after.check_passed == Some(true) => {
                                self.repairs.fixed(&result.content)
                            }
                            None => {}
                        }
                        self.record_change(&call.name, &events).await;
                    }
                }
            }
            if called > 0 && called == fruitless {
                self.barren += 1;
            } else {
                self.barren = 0;
            }
            self.reconsider_model(&record, &events).await;
            let barren = self.barren;
            let tools = record.after.tools.join(", ");
            self.close_step(record);
            // 2 shapes of the same thing.
            // A run of steps where nothing worked at all, and a turn that keeps failing here and
            // there without ever changing anything: the second is what a model that corrupts its
            // own arguments looks like, and it never trips the first.
            let stuck = if barren >= BARREN_STEPS {
                Some(format!("{barren} steps in a row that got nowhere"))
            } else if self.failures >= FAILED_CALLS && self.this_turn.files.is_empty() {
                Some(format!(
                    "{} calls failed and nothing has changed",
                    self.failures
                ))
            } else {
                None
            };
            if let Some(why) = stuck {
                // A turn that changed something before it lost its way left the change behind, and
                // the way back is worth naming while it is still the last one.
                let back = if self.this_turn.files.is_empty() {
                    String::new()
                } else {
                    " /undo puts back what it changed.".to_string()
                };
                let _ = events
                    .send(AgentEvent::Notice(format!(
                        "stopped: {why} (last: {tools}). Say what to do differently.{back}"
                    )))
                    .await;
                self.end_turn(&events, Some(crate::sound::Outcome::Error))
                    .await;
                return Ok(());
            }
        }

        let _ = events
            .send(AgentEvent::Notice(format!(
                "stopped after {} steps (max_steps)",
                self.cfg.max_steps
            )))
            .await;
        self.end_turn(&events, Some(crate::sound::Outcome::Error))
            .await;
        Ok(())
    }

    /// Raise or lower the model from what the last step actually did.
    ///
    /// The signal is objective: the language server read the file the agent had just written and
    /// found something wrong with it.
    /// The cheap path is tried first.
    /// The expensive one is used only where it has been shown to be needed.
    ///
    /// One rung, and down again after 2 clean steps.
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

        // Down again once the work is going well, so the expensive model is paid for where it was
        // needed and not for the rest of the turn.
        // Only a write that checked out counts: a step that read a file says nothing about whether
        // the cheaper model can produce a change that compiles, and taking it as evidence would
        // send the session back down on the strength of two searches.
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
        // A write that did or did not check out is the only thing here worth remembering past the
        // session: it is the evidence the next one starts from.
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
        // Bounded: a long session's steps are on disk, and what is kept here is only what a person
        // or a later decision would look back over.
        self.steps.push(record);
        if self.steps.len() > 200 {
            self.steps.remove(0);
        }
    }

    /// The read the next step is most likely to ask for, done while the model is thinking.
    ///
    /// A small model is asked which single read-only call is coming.
    /// That call is made, and its answer is put where a repeat would find it.
    /// A wrong guess costs time that was being spent waiting anyway.
    /// A right one takes a whole round trip out of the turn.
    ///
    /// Read-only by construction.
    /// A guess that would change something is dropped rather than run.
    fn start_guess(&self, cancel: &CancellationToken) -> Option<Guessing> {
        let model = self.cfg.guess_with.clone()?;
        let stop = cancel.child_token();
        let asked = Guess {
            // At a priority that yields: a guess must never be in front of the answer the person is
            // waiting for, and the cluster can put it on another node.
            backend: self
                .background
                .clone()
                .unwrap_or_else(|| self.backend.clone()),
            registry: self.registry.clone(),
            ctx: {
                // A guess may not ask the person for anything.
                // A question takes attention for a read nobody asked for.
                // The guess is abandoned the moment the answer arrives, which would leave the
                // question on screen,
                // unanswerable.
                //
                // Refused where it is asked rather than filtered by a list of tools.
                // The sender has nothing listening, so anything that would ask is declined,
                // including tools
                // added later.
                let mut ctx = self.ctx.clone();
                let (unheard, _) = tokio::sync::mpsc::channel(1);
                ctx.approval = unheard;
                ctx
            },
            model,
            session: self.session.id.clone(),
            recent: self.recent_for_guess(),
            known: self.repeats.keys().cloned().collect(),
        };
        let token = stop.clone();
        Some((tokio::spawn(async move { asked.run(token).await }), stop))
    }

    /// The tail of the conversation a guess is made from.
    fn recent_for_guess(&self) -> String {
        self.session
            .history
            .iter()
            .rev()
            .take(4)
            .map(|m| {
                let body: String = m.content.chars().take(600).collect();
                format!("{}: {body}", m.role)
            })
            .collect::<Vec<_>>()
            .join("\n")
            .chars()
            .take(REVIEW_ARG_CHARS)
            .collect()
    }

    /// What a second model says about a change, before it is written.
    ///
    /// Asked of a different model than the one proposing the change.
    /// A model checking its own work agrees with itself.
    /// Read-only, one question.
    ///
    /// `None` means no objection.
    /// It is also what a reviewer that could not be reached gives.
    /// A review that fails must not be able to stop the work.
    async fn second_opinion(
        &self,
        call: &ResolvedCall,
        said: &str,
        cancel: &CancellationToken,
    ) -> Option<String> {
        // The last thing the person asked for, which is what the change has to serve.
        let asked = self
            .session
            .history
            .iter()
            .rev()
            .find(|m| m.role == "user")
            .map(|m| m.content.chars().take(REVIEW_ARG_CHARS).collect::<String>())?;
        let args = serde_json::to_string(&call.arguments).ok()?;
        let args: String = args.chars().take(REVIEW_ARG_CHARS).collect();
        let req = TurnRequest {
            model: self.model_for(crate::agent::roles::Role::Review),
            messages: vec![
                Message::system(
                    "You are checking one proposed change against what was asked for. \
                     Answer with the single word OK when the change does what was asked. \
                     Otherwise answer with one short sentence naming what is wrong with \
                     it. Do not suggest improvements and do not comment on style: only \
                     say something when the change would not do what was asked, would \
                     lose work that is already there, or touches the wrong file.",
                ),
                Message::user(format!(
                    "What was asked:\n{asked}\n\nWhat the agent says it is doing:\n{said}\n\n                     The call it wants to make:\n{}: {args}",
                    call.name
                )),
            ],
            tools: None,
            grammar: None,
            max_tokens: 120,
            temperature: 0.0,
            user: Some(format!("{}-review", self.session.id)),
            reason: false,
            purpose: crate::backend::meter::Purpose::Review,
        };
        let answer = tokio::select! {
            biased;
            _ = cancel.cancelled() => return None,
            r = self.backend.complete(req) => r.ok()?,
        };
        let answer = answer.trim();
        // A reviewer that says nothing, says OK, or opens with it has no objection.
        let head = answer.trim_start_matches(['"', '`', '*']).trim_start();
        if head.is_empty()
            || head.len() > REVIEW_SAID_CHARS
            || head.to_lowercase().starts_with("ok")
        {
            return None;
        }
        Some(head.lines().next()?.trim().to_string())
    }

    /// Diagnostics for a file a write tool just changed, when there are any.
    ///
    /// The path is read from the tool's own result rather than from its arguments: what was written
    /// is what the tool says it wrote, and a refused or failed call says nothing to parse.
    /// Never fails a write: a language server that is missing, slow or silent leaves the result as
    /// it was.
    async fn verify_written(
        &self,
        tool: &str,
        out: &ToolOutput,
    ) -> Option<crate::lsp::check::Verdict> {
        use crate::lsp::check::Verdict;
        if out.is_error
            || !matches!(
                tool,
                "write_file" | "edit_file" | "edit_lines" | "apply_patch" | "take_work"
            )
        {
            return None;
        }
        let written = changed_paths(&out.content);
        // The language servers first: they are running, they know the project, and they say what
        // this write changed in what they report.
        let mut unchecked = None;
        if let Some(lsp) = self.lsp.as_ref() {
            let paths: Vec<_> = written.iter().map(|n| self.ctx.root.join(n)).collect();
            lsp.files_changed(&paths).await;
            match crate::lsp::check::after_write(lsp, &self.ctx.root, &paths).await {
                Some(Verdict::NotChecked { why }) => unchecked = Some(why),
                Some(verdict) => return Some(verdict),
                None => {}
            }
        }
        // Underneath them, the compiler, asked to compile and nothing else.
        // Once per step, because a step that writes 3 files is one change.
        if self
            .checked_this_step
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            return unchecked.map(|why| Verdict::NotChecked { why });
        }
        match crate::agent::check::run(&self.ctx.root, &written).await {
            Some(verdict) => Some(verdict),
            None => unchecked.map(|why| Verdict::NotChecked { why }),
        }
    }

    /// Run the turn-end hook, if there is one, and close the turn.
    ///
    /// What the hook says is a notice rather than part of the conversation: the turn is over, so
    /// there is nothing left for the model to do about it, and the person is the one who has to
    /// read that the formatter rewrote 3 files.
    ///
    /// `outcome` is how the turn ended, for the sound declared for its end.
    /// A turn the person interrupted ends with none.
    async fn end_turn(
        &self,
        events: &mpsc::Sender<AgentEvent>,
        outcome: Option<crate::sound::Outcome>,
    ) {
        // What the turn came to, in one line: how long, how many steps, what it cost and what it
        // changed.
        // All 4 were known and thrown away.
        if let Some(receipt) = self.this_turn.receipt() {
            let _ = events.send(AgentEvent::Receipt(receipt)).await;
        }
        // What the turn's requests drew, by purpose, kept with the conversation so the cost of a
        // session can be read back from its file.
        let energy = self.this_turn.energy().by_purpose();
        if !energy.is_empty() {
            if let Some(t) = self.session.transcript() {
                t.append(crate::agent::transcript::Record::Energy(energy));
            }
        }
        // Written at the end of a turn rather than after every step: losing it costs a memory, not
        // a session, and a turn is the unit a person would call a piece of work anyway.
        self.history.save();
        // The hooks run here when the turn ended some other way than an answer, and what they said
        // at the answer is told now.
        let rans = match self.this_turn.turn_end.clone() {
            Some(rans) => rans,
            None => {
                crate::agent::hooks::run_all(
                    &self.cfg.hooks.turn_end,
                    crate::agent::hooks::Event::TurnEnd,
                    &self.ctx.root,
                    None,
                    self.hook_input(crate::agent::hooks::Event::TurnEnd, serde_json::json!({})),
                )
                .await
            }
        };
        if let Some(text) = crate::agent::hooks::said(crate::agent::hooks::Event::TurnEnd, &rans) {
            let _ = events.send(AgentEvent::Notice(text)).await;
        }
        if let Some(outcome) = outcome {
            crate::sound::play(crate::sound::Event::TurnEnd, outcome);
        }
        let _ = events.send(AgentEvent::TurnDone).await;
    }

    /// Close an unanswered turn.
    ///
    /// The user message is already in the history.
    /// Without a reply behind it the next turn puts 2 `user` roles in a row.
    /// The compactor's boundary search does not expect that shape.
    async fn abandon(&mut self, events: &mpsc::Sender<AgentEvent>, why: &str) {
        self.session.push(Message::assistant(TURN_ABANDONED));
        let _ = events.send(AgentEvent::Notice(why.to_string())).await;
        // The hook runs here too: an abandoned turn can have changed files, and that is exactly
        // when a formatter or a test suite is worth running.
        let outcome = (why != "interrupted").then_some(crate::sound::Outcome::Error);
        self.end_turn(events, outcome).await;
    }

    /// How many times turn-end hooks may keep one turn going.
    const HELD_AT_MOST: u8 = 2;

    /// How many refused proofs of one turn raise the session to the stronger model.
    const PROOFS_REFUSED_AT_MOST: u8 = 2;

    /// What agents did in their own copies and is still to be brought in, when the turn may still
    /// be kept going for it.
    /// Only the session that handed the work on is held.
    fn held_by_work(&mut self) -> Option<String> {
        if self.cfg.delegated || self.this_turn.held >= Self::HELD_AT_MOST {
            return None;
        }
        let waiting: Vec<String> = self
            .ctx
            .works
            .lock()
            .ok()?
            .values()
            .map(|w| format!("work {} of the agent {}", w.id, w.agent))
            .collect();
        if waiting.is_empty() {
            return None;
        }
        self.this_turn.held += 1;
        Some(format!("{} waits to be brought in", waiting.join(", ")))
    }

    /// Run the turn-end hooks on the answer.
    /// Returns the reason they give when one blocks and the turn may still be kept going.
    /// Otherwise what they said is kept for the end of the turn.
    async fn held_by_turn_end(&mut self, answer: &str) -> Option<String> {
        if self.cfg.hooks.turn_end.is_empty() {
            return None;
        }
        let rans = crate::agent::hooks::run_all(
            &self.cfg.hooks.turn_end,
            crate::agent::hooks::Event::TurnEnd,
            &self.ctx.root,
            None,
            self.hook_input(
                crate::agent::hooks::Event::TurnEnd,
                serde_json::json!({"answer": answer}),
            ),
        )
        .await;
        match rans.iter().find(|r| r.blocked) {
            Some(r) if self.this_turn.held < Self::HELD_AT_MOST => {
                self.this_turn.held += 1;
                Some(r.output.clone())
            }
            _ => {
                self.this_turn.turn_end = Some(rans);
                None
            }
        }
    }

    /// Tell the session-end hooks the session is over.
    /// What they say is not kept: nobody is left to read it in the conversation.
    pub async fn end_session(&self) {
        if self.cfg.delegated || self.cfg.hooks.session_end.is_empty() {
            return;
        }
        use crate::agent::hooks::{run_all, Event};
        run_all(
            &self.cfg.hooks.session_end,
            Event::SessionEnd,
            &self.ctx.root,
            None,
            self.hook_input(Event::SessionEnd, serde_json::json!({})),
        )
        .await;
    }

    /// What a hook is given: the event, the session and the project, with `facts`.
    fn hook_input(&self, event: crate::agent::hooks::Event, facts: Value) -> Value {
        let mut input = serde_json::json!({
            "event": event.label(),
            "session_id": self.session.id,
            "root": self.ctx.root,
        });
        if let (Some(input), Some(facts)) = (input.as_object_mut(), facts.as_object()) {
            // The fields other tools' hooks read, beside lokode's own.
            if let Some(extra) =
                crate::claude::hook_fields(event.label(), &facts.clone().into()).as_object()
            {
                input.extend(extra.clone());
            }
            input.insert("cwd".into(), serde_json::json!(self.ctx.root));
            input.extend(facts.clone());
        }
        input
    }

    /// Take again the tools of each server that said they changed, so this turn offers what the
    /// servers offer now.
    async fn take_changed_tools(&mut self, events: &mpsc::Sender<AgentEvent>) {
        let changed: Vec<_> = crate::mcp::snapshot(&self.mcp)
            .into_iter()
            .filter(|c| c.take_tools_changed())
            .collect();
        if changed.is_empty() {
            return;
        }
        let mut registry = (*self.registry).clone();
        for client in changed {
            let said = match crate::mcp::readopt(&client, &mut registry).await {
                Ok((n, notices)) => {
                    let mut said = format!("{} changed its tools: {n} now", client.server);
                    for notice in notices {
                        said.push_str(&format!("; {notice}"));
                    }
                    said
                }
                Err(e) => format!(
                    "{} changed its tools and would not list them: {e:#}",
                    client.server
                ),
            };
            let _ = events.send(AgentEvent::Notice(said)).await;
        }
        self.registry = Arc::new(registry);
        self.rebuild_preamble();
    }

    /// Have the note left where steps are elided carry the task list, so the model keeps the goal
    /// when the call that set it is elided.
    async fn carry_tasks(&mut self) {
        let carry = match &self.tasks {
            Some(list) => {
                let tasks = list.get().await;
                (!tasks.is_empty()).then(|| {
                    format!(
                        "The task list as it stood:\n{}",
                        crate::tools::plan::TaskList::render(&tasks)
                    )
                })
            }
            None => None,
        };
        self.session.set_carry(carry);
    }

    /// The agent a line opens on with `@`, and the rest of the line, when the agent is one the
    /// delegation knows and no file of the project has that name.
    fn agent_named<'a>(&self, line: &'a str) -> Option<(&'a str, &'a str)> {
        let rest = line.trim_start().strip_prefix('@')?;
        let (name, task) = rest.split_once(char::is_whitespace)?;
        if self.cfg.delegated || self.ctx.root.join(name).exists() {
            return None;
        }
        let schema = self.registry.get("delegate")?.args_schema();
        let known = schema["properties"]["agent"]["enum"]
            .as_array()?
            .iter()
            .any(|n| n.as_str() == Some(name));
        known.then_some((name, task.trim()))
    }

    /// Commit what a tool just changed, if a history is being kept.
    ///
    /// A failure to record is reported and not fatal: losing the ability to undo is bad, and
    /// refusing to continue the turn over it would be worse.
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
        let stranded = stranded_reasoning(&completion).map(str::to_string);
        match self.cfg.protocol {
            ToolProtocol::Native => {
                let mut calls = Vec::new();
                for (id, name, arguments) in completion.calls.finish() {
                    // The arguments come over as a string of JSON the model wrote, so they are
                    // wrong in the same ways the envelope's are, and earn the same tolerant parse.
                    // What is not repaired here is the name: a call to a tool that does not exist
                    // is refused by `dispatch_tool`, which tells the model so and lets it try
                    // again.
                    let arguments = repair::tolerant_parse(&arguments).unwrap_or(Value::Null);
                    calls.push(ResolvedCall {
                        // A server that omits the id leaves us to invent one.
                        // The invented id is only matched against the result returned for it.
                        id: id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                        name,
                        arguments,
                    });
                }
                if let Some(reasoning) = stranded.as_deref() {
                    calls = self.calls_from_reasoning(reasoning, events).await;
                }
                Ok(ModelTurn {
                    stranded: calls.is_empty()
                        && stranded.as_deref().is_some_and(looks_like_a_call),
                    text: completion.text,
                    calls,
                })
            }
            ToolProtocol::Envelope => {
                // An envelope written in the reasoning is read as the answer when it is valid
                // as it stands.
                // It is never sent to repair: the reasoning holds prose a repair would turn into
                // an answer.
                let in_reasoning = stranded
                    .as_deref()
                    .and_then(|r| repair::tolerant_parse(r).ok())
                    .filter(|v| validator.is_valid(v))
                    .and_then(|v| serde_json::from_value::<TurnEnvelope>(v).ok())
                    .filter(|e| !e.tool_calls.is_empty());
                let envelope = match in_reasoning {
                    Some(envelope) => {
                        self.say_calls_were_in_reasoning(events).await;
                        envelope
                    }
                    None => {
                        let calls = match stranded.as_deref() {
                            Some(reasoning) => self.calls_from_reasoning(reasoning, events).await,
                            None => Vec::new(),
                        };
                        if !calls.is_empty() {
                            return Ok(ModelTurn {
                                text: String::new(),
                                calls,
                                stranded: false,
                            });
                        }
                        self.parse_with_repair(&completion.text, schema, validator, cancel, events)
                            .await?
                    }
                };
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
                    stranded: false,
                })
            }
        }
    }

    /// The calls written in `reasoning`, each with an id of its own.
    /// The person is told when there are any.
    async fn calls_from_reasoning(
        &self,
        reasoning: &str,
        events: &mpsc::Sender<AgentEvent>,
    ) -> Vec<ResolvedCall> {
        let calls: Vec<ResolvedCall> = calls_in_reasoning(reasoning, |name| {
            self.registry.get(name).map(|t| t.args_schema())
        })
        .into_iter()
        .map(|(name, arguments)| ResolvedCall {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            arguments,
        })
        .collect();
        if !calls.is_empty() {
            self.say_calls_were_in_reasoning(events).await;
        }
        calls
    }

    async fn say_calls_were_in_reasoning(&self, events: &mpsc::Sender<AgentEvent>) {
        tracing::warn!("the answer was empty and its reasoning held the tool calls");
        let _ = events
            .send(AgentEvent::Notice(
                "the model wrote its tool calls in its reasoning; they are run as calls".into(),
            ))
            .await;
    }

    /// Put the assistant's turn into the history in the form the protocol expects to see it again.
    ///
    /// The model is shown its own previous turn on the next request, so it has to be recorded the
    /// way that protocol reads: `tool_calls` on the message natively, and the serialised envelope
    /// otherwise.
    /// Recording it in the other one's shape is how a model ends up unable to see what it just
    /// asked for.
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
        // The grammar constrains the envelope.
        // Under the native protocol there is no envelope to constrain, and forcing this schema
        // would demand an object the model is not being asked to produce.
        // Asked for only when the provider can actually hold the model to it.
        // A server that cannot enforce a schema is not sent one: the request would be accepted and
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
            reason: true,
            purpose: crate::backend::meter::Purpose::Step,
        };

        let mut stream: ChunkStream = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(anyhow::anyhow!("interrupted")),
            r = self.backend.stream(req) => r?,
        };

        let mut out = Completion::default();
        // Said once each: the phase is what the status line holds until the next one, not a line
        // per token.
        let (mut answering, mut thinking) = (false, false);
        // The first piece is waited for differently from the ones after it: reading the prompt
        // takes as long as the prompt is long, and a stall is a stall whatever was asked.
        let mut patience = first_token_timeout(self.session.size_chars());
        loop {
            let next = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(anyhow::anyhow!("interrupted")),
                // A server that stops speaking mid-answer would otherwise hold the turn until
                // the client gives up, after `READ_TIMEOUT`.
                // The interface would say it is busy for all that time, and it would not be.
                _ = tokio::time::sleep(patience) => {
                    return Err(anyhow::anyhow!(
                        "backend sent nothing for {}s",
                        patience.as_secs()
                    ))
                }
                p = stream.next() => match p {
                    Some(p) => p,
                    None => break,
                },
            };
            // Something arrived, so the server is at work: from here a silence is a stall.
            patience = STREAM_IDLE_TIMEOUT;
            match next? {
                StreamEvent::Alive => {}
                StreamEvent::Prefill { done, total } => {
                    let _ = events.send(AgentEvent::Prefill { done, total }).await;
                }
                StreamEvent::Delta(p) => {
                    if !answering {
                        answering = true;
                        let _ = events.send(AgentEvent::Phase(Phase::Answering)).await;
                    }
                    out.text.push_str(&p);
                    let _ = events.send(AgentEvent::AssistantDelta(p)).await;
                }
                // Shown as it arrives and then dropped.
                // It is deliberately not appended to `out.text`: the scratchpad is not the answer,
                // and replaying it into the history would teach the next turn that it was.
                StreamEvent::Reasoning(p) => {
                    if !answering && !thinking {
                        thinking = true;
                        let _ = events.send(AgentEvent::Phase(Phase::Thinking)).await;
                    }
                    out.reasoning.push_str(&p);
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

    /// Parse the envelope, asking the model to reshape it only when it will not parse.
    ///
    /// The phase is announced from here rather than before the call: said unconditionally, it
    /// appeared on every step of every turn, and a screen that says "repairing" when nothing is
    /// wrong teaches its reader to ignore it.
    async fn parse_with_repair(
        &mut self,
        raw: &str,
        schema: &Value,
        validator: &Validator,
        cancel: &CancellationToken,
        events: &mpsc::Sender<AgentEvent>,
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
            let _ = events.send(AgentEvent::Phase(Phase::Repairing)).await;
            tracing::warn!(
                attempt = attempt + 1,
                max = MAX_REPAIR_ATTEMPTS,
                why = %last_err,
                "asking the model to repair its own output"
            );
            let prompt = repair::repair_prompt(&last_err.to_string(), &current);
            let repair_req = TurnRequest {
                // A malformed reply has to be reshaped, not thought about, so this is the one call
                // in the loop whose model can be the smallest one available.
                model: self.model_for(crate::agent::roles::Role::Repair),
                messages: vec![
                    Message::system("You are a strict JSON formatter. Output a single JSON object only — no prose, no fences."),
                    Message::user(prompt),
                ],
                // The repair round-trip asks for one thing: valid JSON.
                // Offering tools here would invite the model to do something else instead.
                tools: None,
                grammar: self.backend.capabilities().grammar.then(|| schema.clone()),
                max_tokens: 4096,
                temperature: 0.0,
                user: Some(format!("{}-repair", self.session.id)),
                reason: true,
                purpose: crate::backend::meter::Purpose::Repair,
            };
            current = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(anyhow::anyhow!("interrupted")),
                r = self.backend.complete(repair_req) => r?,
            };
        }
        unreachable!()
    }

    async fn dispatch_tool(&self, id: &str, name: &str, mut args: Value) -> ToolOutput {
        // The call's own context: what it prints carries the id of its step and fills a tail of its
        // own, and it can be stopped alone, the turn going on without it.
        let ctx = crate::tools::registry::ToolCtx {
            call: Some(id.to_string()),
            cancel: self.ctx.cancel.child_token(),
            tail: Default::default(),
            ..self.ctx.clone()
        };
        let Some(tool) = self.registry.get(name) else {
            tracing::warn!(tool = name, "model called a tool that does not exist");
            // Named rather than merely refused.
            // A model that invents `list_files` means `list_dir`, and a refusal that says only that
            // the name is wrong costs a step to guess again, often to the next invented name.
            return ToolOutput::err(match nearest_tool(&self.registry, name) {
                Some(near) => format!("no tool called '{name}'; did you mean '{near}'?"),
                None => format!("no tool called '{name}'"),
            });
        };
        // Refused here rather than at the approval prompt: planning is a question about what would
        // happen, and answering it must not depend on somebody declining 15 times.
        // The refusal names the mode so the model plans instead of retrying, and reads as this
        // agent's rule rather than as the user saying no.
        if self.cfg.mode == Mode::Plan && tool.mutates() {
            tracing::info!(tool = name, "refused while planning");
            return ToolOutput::err(format!(
                "planning only: '{name}' would change something, so it was not run. \
                 Say what you would do with it as a step of the plan."
            ));
        }
        // Around a call that changes something, and around the reads a hook names: a hook on every
        // read would run on the searches an agent does by the 12.
        use crate::agent::hooks::{run_all, said, Event};
        let target = Some((name, tool.mutates()));
        let before = run_all(
            &self.cfg.hooks.pre_tool,
            Event::PreTool,
            &self.ctx.root,
            target,
            self.hook_input(
                Event::PreTool,
                serde_json::json!({"tool": name, "args": args}),
            ),
        )
        .await;
        if let Some(refusal) = before.iter().find(|r| r.blocked) {
            return ToolOutput::err(format!(
                "refused before it ran, by a pre-tool hook:\n{}",
                refusal.output
            ));
        }
        if let Some(changed) = before.iter().rev().find_map(|r| r.args.clone()) {
            args = changed;
        }
        let args_seen = args.clone();
        let started = std::time::Instant::now();
        ctx.offer_stop(name).await;
        let out = match tool.run(&ctx, args).await {
            Ok(o) => o,
            Err(e) => ToolOutput::err(format!("tool '{name}' failed: {e:#}")),
        };
        if ctx.cancel.is_cancelled() && !self.ctx.cancel.is_cancelled() {
            return ToolOutput::err("the person stopped this call before it finished");
        }
        // What the compiler already knows about the file that was just written, handed back in the
        // same result.
        // Without it the model learns of its own syntax error on the next turn, if it thinks to
        // ask, and usually it does not.
        let out = match self.verify_written(name, &out).await {
            Some(verdict) => {
                use crate::lsp::check::Verdict;
                let mut out = out;
                let said = match verdict {
                    Verdict::Failed { report, .. } => format!("{CHECK_FAILED}\n{report}"),
                    Verdict::Passed { by, notes } if notes.is_empty() => {
                        format!("{CHECK_PASSED} {by}: no new errors")
                    }
                    Verdict::Passed { by, notes } => {
                        format!("{CHECK_PASSED} {by}: no new errors\n{notes}")
                    }
                    Verdict::NotChecked { why } => format!("{NOT_CHECKED} {why}"),
                };
                out.content.push_str(&format!("\n\n{said}"));
                out
            }
            None => out,
        };
        // What the hooks say is handed over whether they succeeded or not: a formatter that rewrote
        // the file has something the model needs to know.
        let after = run_all(
            &self.cfg.hooks.post_tool,
            Event::PostTool,
            &self.ctx.root,
            target,
            self.hook_input(
                Event::PostTool,
                serde_json::json!({
                    "tool": name,
                    "args": args_seen,
                    "result": {"content": out.content, "is_error": out.is_error},
                }),
            ),
        )
        .await;
        let mut out = out;
        for (event, rans) in [(Event::PreTool, &before), (Event::PostTool, &after)] {
            if let Some(text) = said(event, rans) {
                out.content.push_str(&format!("\n\n{text}"));
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
    /// A guess must not be able to put a question to the person.
    ///
    /// A tool that asks for approval takes attention for a read nobody asked for.
    /// The guess is abandoned the instant the real answer arrives, which leaves the question on
    /// screen.
    /// A guessed web fetch is read-only, so it passes the mutation test, and still asks before it
    /// reaches the network.
    #[tokio::test]
    async fn a_guess_cannot_ask_for_an_approval() {
        use crate::sandbox::approval::{ApprovalRequest, ApprovalScope};
        // The sender a guess is given: nothing is listening on it.
        let (unheard, _) = tokio::sync::mpsc::channel(1);
        let asked = crate::sandbox::approval::ask(
            &unheard,
            ApprovalRequest {
                tool: "web_fetch".into(),
                summary: "GET https://example.com".into(),
                diff_preview: None,
                scope: ApprovalScope::Host("example.com".into()),
            },
        )
        .await;
        assert!(
            matches!(asked, crate::sandbox::approval::ApprovalDecision::Deny(_)),
            "a guess with nobody listening must be refused, not left waiting"
        );
    }

    /// Reading the prompt is work, not silence: a window asked for precisely so that a large prompt
    /// can be sent is unusable if the client gives up while it is read.
    #[test]
    fn a_long_prompt_is_waited_for_longer_than_a_stall() {
        // A short prompt is judged by the gap rule, as it always was.
        assert_eq!(first_token_timeout(0), STREAM_IDLE_TIMEOUT);
        assert_eq!(first_token_timeout(10_000), STREAM_IDLE_TIMEOUT);
        // A window's worth of prompt is waited through.
        let full = first_token_timeout(131_072 * crate::agent::session::CHARS_PER_TOKEN);
        assert!(full > STREAM_IDLE_TIMEOUT, "{full:?}");
        assert_eq!(full.as_secs(), (131_072 / SLOWEST_PREFILL) as u64);
    }

    use super::*;

    #[test]
    fn a_turn_counts_every_file_its_write_tools_named() {
        let mut tally = TurnTally::begin();
        for path in changed_paths("wrote src/a.rs (120 bytes)\nsome diagnostic") {
            tally.touched(&path);
        }
        for path in changed_paths("edited src/b.rs\n@@ -1 +1 @@") {
            tally.touched(&path);
        }
        for path in changed_paths("applied 3 edit(s) to src/a.rs") {
            tally.touched(&path);
        }
        for path in changed_paths("patched src/c.rs\npatched src/d.rs") {
            tally.touched(&path);
        }
        // A file written twice is one file, and a diagnostic line is not a file.
        assert_eq!(
            tally.files,
            vec!["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs"]
        );
    }

    #[test]
    fn a_refused_write_names_nothing() {
        assert!(changed_paths("refused: src/a.rs is outside the workspace").is_empty());
        assert!(changed_paths("src/a.rs: write failed: permission denied").is_empty());
    }

    #[test]
    fn a_quick_answer_that_changed_nothing_gets_no_receipt() {
        let tally = TurnTally {
            steps: 1,
            started: Some(std::time::Instant::now()),
            ..Default::default()
        };
        assert_eq!(tally.describe(), None);
    }

    #[test]
    fn a_turn_that_changed_a_file_says_so() {
        let mut tally = TurnTally::begin();
        tally.steps = 3;
        tally.tokens_in = 900;
        tally.tokens_out = 40;
        tally.touched("src/a.rs");
        let line = tally.describe().expect("a turn with work reports it");
        assert!(line.contains("3 step(s)"), "{line}");
        assert!(line.contains("900 in, 40 out"), "{line}");
        assert!(line.contains("changed src/a.rs"), "{line}");
    }

    use crate::tools::fs_read::ListDir;
    use crate::tools::harness::{Answer, Harness};
    use std::sync::{Arc as StdArc, Mutex};

    /// A backend that answers a queue of canned responses, and remembers what it was sent.
    ///
    /// Enough of a server to drive the loop through a real request, a real stream and a real second
    /// step.
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
                // Read until the body has arrived.
                // The request declares its length.
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
                // The model list is asked once per model and is not part of a script: it states no
                // window, and the queue is kept for the turns.
                let response = if buf.starts_with(b"GET /v1/models") {
                    let body = r#"{"object":"list","data":[]}"#;
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    )
                } else {
                    queue.next().unwrap_or_else(|| {
                        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n".into()
                    })
                };
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

    /// A plain JSON reply, for the calls that do not stream.
    fn json_reply(content: &str) -> String {
        let body = serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": content},
                         "finish_reason": "stop"}]
        })
        .to_string();
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
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
                guess_with: None,
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
                shared_protocol: None,
                delegated: false,
            },
            ctx: h.ctx.clone(),
            checkpoints: None,
            tasks: None,
            mcp: Default::default(),
            turn: 0,
        };
        (agent, steer_tx)
    }

    async fn drive(agent: &mut Agent, prompt: &str) -> Vec<AgentEvent> {
        agent
            .collect_turn(prompt.into(), CancellationToken::new())
            .await
            .unwrap()
    }

    /// A file named in the question arrives with it, so the model does not spend a round trip
    /// fetching what the person had already decided was relevant.
    /// A call stopped from the screen ends alone: it says the person stopped it, and the turn it
    /// belongs to is not abandoned.
    #[tokio::test]
    async fn a_call_stops_alone() {
        struct Waits;
        #[async_trait::async_trait]
        impl crate::tools::registry::Tool for Waits {
            fn name(&self) -> &str {
                "waits"
            }
            fn description(&self) -> &str {
                "waits until it is stopped"
            }
            fn args_schema(&self) -> Value {
                serde_json::json!({"type": "object"})
            }
            async fn run(
                &self,
                ctx: &crate::tools::registry::ToolCtx,
                _: Value,
            ) -> anyhow::Result<ToolOutput> {
                ctx.cancel.cancelled().await;
                Ok(ToolOutput::ok("ended"))
            }
        }
        let h = Harness::new(Answer::Yes);
        let backend = fake_backend(Vec::new()).await;
        let mut reg = Registry::new();
        reg.register(Waits);
        let mut agent = agent_with(&h, &backend, ToolProtocol::Native, reg);
        let (live, mut screen) = tokio::sync::mpsc::channel(8);
        agent.ctx.live = Some(live);
        let turn = agent.ctx.cancel.clone();
        let agent = std::sync::Arc::new(agent);
        let running = agent.clone();
        let call = tokio::spawn(async move {
            running
                .dispatch_tool("1", "waits", serde_json::json!({}))
                .await
        });
        let stop = loop {
            let told = screen.recv().await.expect("the call tells the screen");
            if let crate::tools::registry::Live::Stop(stop) = told.what {
                assert_eq!(told.call.as_deref(), Some("1"));
                break stop;
            }
        };
        stop.cancel();
        let out = call.await.unwrap();
        assert!(out.is_error);
        assert_eq!(
            out.content,
            "the person stopped this call before it finished"
        );
        assert!(!turn.is_cancelled(), "the turn goes on");
    }

    #[tokio::test]
    async fn a_file_named_in_the_question_arrives_with_it() {
        let h = Harness::new(Answer::Yes);
        h.write("notes.md", "the answer is in here");
        let backend = fake_backend(vec![sse(&[
            r#"{"choices":[{"delta":{"content":"read it"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        ])])
        .await;
        let mut agent = agent_for(&h, &backend, ToolProtocol::Native);
        let events = drive(&mut agent, "what does @notes.md say?").await;

        let sent = backend.seen.lock().unwrap();
        let messages = sent[0]["messages"].as_array().unwrap();
        let user = messages
            .iter()
            .find(|m| m["role"] == "user")
            .and_then(|m| m["content"].as_str())
            .unwrap_or_default();
        assert!(user.contains("what does @notes.md say?"), "{user}");
        assert!(user.contains("the answer is in here"), "{user}");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Notice(s) if s.contains("notes.md"))),
            "the user was not told what went with the question: {events:?}"
        );
    }

    /// An error the server sends inside the stream ends the turn as a backend error.
    /// The notice carries the server's own message.
    #[tokio::test]
    async fn an_error_inside_the_stream_is_a_backend_error() {
        let h = Harness::new(Answer::Yes);
        let backend = fake_backend(vec![sse(&[
            r#"{"error":{"message":"STREAMING PREFILL: out of memory","type":"server_error"}}"#,
        ])])
        .await;
        let mut agent = agent_for(&h, &backend, ToolProtocol::Native);
        let events = drive(&mut agent, "hello").await;
        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::Notice(s)
                if s.contains("backend error") && s.contains("STREAMING PREFILL: out of memory"))),
            "the error was not reported: {events:?}"
        );
    }

    /// A call the server put in the reasoning, with an empty answer, is run as a call.
    #[tokio::test]
    async fn a_call_left_in_the_reasoning_is_run() {
        let h = Harness::new(Answer::Yes);
        let backend = fake_backend(vec![
            sse(&[
                r#"{"choices":[{"delta":{"reasoning_content":"Let me look.\n<tool_call>\n<function=list_dir>\n<parameter=path>\n.\n</parameter>\n</function>\n</tool_call>"}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ]),
            sse(&[
                r#"{"choices":[{"delta":{"content":"done"}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ]),
        ])
        .await;
        let mut agent = agent_for(&h, &backend, ToolProtocol::Native);
        let events = drive(&mut agent, "list the files").await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolCall { name, .. } if name == "list_dir")),
            "the call was not run: {events:?}"
        );
        let sent = backend.seen.lock().unwrap();
        assert_eq!(
            sent.len(),
            2,
            "the turn ended before the call's result was sent"
        );
        let last = sent[1]["messages"].as_array().unwrap().last().unwrap();
        assert_eq!(last["role"], "tool", "{last}");
    }

    /// The envelope protocol's object written in the reasoning is read as the answer.
    #[tokio::test]
    async fn an_envelope_left_in_the_reasoning_is_read() {
        let h = Harness::new(Answer::Yes);
        let envelope = serde_json::json!({
            "assistant_text": "looking",
            "tool_calls": [{"name": "list_dir", "arguments": {"path": "."}}],
        })
        .to_string();
        let chunk = serde_json::json!({"choices": [{"delta": {"reasoning_content": envelope}}]})
            .to_string();
        let backend = fake_backend(vec![
            sse(&[
                &chunk,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ]),
            sse(&[
                r#"{"choices":[{"delta":{"content":"{\"assistant_text\":\"done\",\"tool_calls\":[]}"}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ]),
        ])
        .await;
        let mut agent = agent_for(&h, &backend, ToolProtocol::Envelope);
        let events = drive(&mut agent, "list the files").await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolCall { name, .. } if name == "list_dir")),
            "the call was not run: {events:?}"
        );
    }

    /// A call in the reasoning that cannot be read is asked for again, once.
    #[tokio::test]
    async fn an_unreadable_call_in_the_reasoning_is_asked_for_again() {
        let h = Harness::new(Answer::Yes);
        let stranded = sse(&[
            r#"{"choices":[{"delta":{"reasoning_content":"<tool_call>\n<function=list_files>\n</function>\n</tool_call>"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        ]);
        let backend = fake_backend(vec![stranded.clone(), stranded]).await;
        let mut agent = agent_for(&h, &backend, ToolProtocol::Native);
        let events = drive(&mut agent, "list the files").await;
        let sent = backend.seen.lock().unwrap();
        assert_eq!(sent.len(), 2, "asked again not once: {events:?}");
        let last = sent[1]["messages"].as_array().unwrap().last().unwrap();
        assert_eq!(last["content"], RESTATE_CALL);
    }

    /// Planning is long and ends in acting, so the model acting will use is asked for while the
    /// plan is being written.
    /// The request goes out and the turn does not wait on it: warming that delayed anything would
    /// cost more than the load it hides.
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
        // The warming request is its own task.
        // It has arrived by the time the fake backend has seen a second request.
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

    /// A delegated question runs in a session of its own and brings back only what it said: the
    /// caller's window holds the answer, not the search that found it.
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
        reg.register(delegation(&backend.url, (*sub).clone(), 4));
        let mut agent = agent_with(&h, &backend, ToolProtocol::Native, reg);
        let events = drive(&mut agent, "what does a.txt say").await;

        let result = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::ToolResult { name, output, .. } if name == "delegate" => Some(output),
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

    /// The delegation of the tests: the given tools, the test backend, `max_steps` steps.
    fn delegation(
        backend: &str,
        registry: crate::tools::registry::Registry,
        max_steps: usize,
    ) -> crate::tools::delegate::Delegate {
        let mut explore = crate::agent::agents::AgentSpec::explore();
        explore.max_steps = Some(max_steps);
        crate::tools::delegate::Delegate::new(Arc::new(crate::tools::delegate::Delegation {
            backend: Arc::new(crate::backend::client::BackendClient::new(
                backend.to_string(),
            )),
            backends: Default::default(),
            registry: Arc::new(registry),
            agents: vec![explore],
            search_model: "test".into(),
            edit_model: "test".into(),
            roles: Default::default(),
            models: Default::default(),
            context_tokens: 8192,
            max_answer_tokens: 4096,
            protocol: Default::default(),
            grammar_enabled: false,
            max_steps: 16,
            slots: Arc::new(tokio::sync::Semaphore::new(2)),
            parallel: 2,
            hooks: Default::default(),
            lsp: None,
            answers: None,
            depth: 1,
            works: Default::default(),
        }))
    }

    /// A delegated session changes nothing, even when it asks to and is given the tool.
    #[tokio::test]
    async fn a_delegated_session_cannot_write() {
        let h = Harness::new(Answer::Yes);
        let backend = fake_backend(vec![
            sse(&[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"delegate","arguments":"{\"task\": \"make b.txt\"}"}}]}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ]),
            sse(&[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"s1","function":{"name":"write_file","arguments":"{\"path\": \"b.txt\", \"content\": \"x\"}"}}]}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ]),
            sse(&[
                r#"{"choices":[{"delta":{"content":"could not write it"}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ]),
            sse(&[
                r#"{"choices":[{"delta":{"content":"done"}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ]),
        ])
        .await;
        let mut reg = crate::boot::build_registry(h.root.clone()).0;
        reg.register(delegation(&backend.url, reg.clone(), 4));
        let mut agent = agent_with(&h, &backend, ToolProtocol::Native, reg);
        drive(&mut agent, "go").await;
        assert!(
            !h.path("b.txt").exists(),
            "a delegated session wrote a file"
        );
    }

    /// An agent named with a tool that writes writes, under the caller's approvals, and writes
    /// nothing while the caller plans.
    #[tokio::test]
    async fn an_agent_with_a_writing_tool_writes_unless_the_caller_plans() {
        for planning in [false, true] {
            let h = Harness::new(Answer::Yes);
            let backend = fake_backend(vec![
                sse(&[
                    r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"delegate","arguments":"{\"agent\": \"scribe\", \"task\": \"make b.txt\"}"}}]}}]}"#,
                    r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
                ]),
                sse(&[
                    r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"s1","function":{"name":"write_file","arguments":"{\"path\": \"b.txt\", \"content\": \"x\"}"}}]}}]}"#,
                    r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
                ]),
                sse(&[
                    r#"{"choices":[{"delta":{"content":"made it"}}]}"#,
                    r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
                ]),
                sse(&[
                    r#"{"choices":[{"delta":{"content":"done"}}]}"#,
                    r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
                ]),
            ])
            .await;
            let mut reg = crate::boot::build_registry(h.root.clone()).0;
            let scribe = crate::agent::agents::parse(
                "scribe",
                "---\ntools: write_file\n---\nWrite what you are asked to.",
                "test",
            )
            .unwrap();
            reg.register(crate::tools::delegate::Delegate::new(Arc::new(
                crate::tools::delegate::Delegation {
                    backend: Arc::new(crate::backend::client::BackendClient::new(
                        backend.url.clone(),
                    )),
                    backends: Default::default(),
                    registry: Arc::new(reg.clone()),
                    agents: vec![crate::agent::agents::AgentSpec::explore(), scribe],
                    search_model: "test".into(),
                    edit_model: "test".into(),
                    roles: Default::default(),
                    models: Default::default(),
                    context_tokens: 8192,
                    max_answer_tokens: 4096,
                    protocol: Default::default(),
                    grammar_enabled: false,
                    max_steps: 4,
                    slots: Arc::new(tokio::sync::Semaphore::new(2)),
                    parallel: 2,
                    hooks: Default::default(),
                    lsp: None,
                    answers: None,
                    depth: 1,
                    works: Default::default(),
                },
            )));
            let mut agent = agent_with(&h, &backend, ToolProtocol::Native, reg);
            agent.ctx.planning = planning;
            drive(&mut agent, "go").await;
            assert_eq!(h.path("b.txt").exists(), !planning, "planning: {planning}");
        }
    }

    /// An agent that names another backend runs there, and the session's backend is asked nothing
    /// on its behalf.
    #[tokio::test]
    async fn an_agent_runs_on_the_backend_it_names() {
        let h = Harness::new(Answer::Yes);
        let here = fake_backend(vec![
            sse(&[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"delegate","arguments":"{\"agent\": \"far\", \"task\": \"say where you are\"}"}}]}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ]),
            sse(&[
                r#"{"choices":[{"delta":{"content":"done"}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ]),
        ])
        .await;
        let there = fake_backend(vec![sse(&[
            r#"{"choices":[{"delta":{"content":"from elsewhere"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        ])])
        .await;
        let mut reg = crate::boot::build_registry(h.root.clone()).0;
        let far = crate::agent::agents::parse(
            "far",
            "---\nbackend: far\nmodel: big\n---\nAnswer.",
            "test",
        )
        .unwrap();
        let mut backends: std::collections::HashMap<
            String,
            Arc<dyn crate::backend::provider::Provider>,
        > = Default::default();
        backends.insert(
            "far".into(),
            Arc::new(crate::backend::client::BackendClient::new(
                there.url.clone(),
            )),
        );
        reg.register(crate::tools::delegate::Delegate::new(Arc::new(
            crate::tools::delegate::Delegation {
                backend: Arc::new(crate::backend::client::BackendClient::new(here.url.clone())),
                backends,
                registry: Arc::new(reg.clone()),
                agents: vec![crate::agent::agents::AgentSpec::explore(), far],
                search_model: "test".into(),
                edit_model: "test".into(),
                roles: Default::default(),
                models: Default::default(),
                context_tokens: 8192,
                max_answer_tokens: 4096,
                protocol: Default::default(),
                grammar_enabled: false,
                max_steps: 4,
                slots: Arc::new(tokio::sync::Semaphore::new(2)),
                parallel: 2,
                hooks: Default::default(),
                lsp: None,
                answers: None,
                depth: 1,
                works: Default::default(),
            },
        )));
        let mut agent = agent_with(&h, &here, ToolProtocol::Native, reg);
        let events = drive(&mut agent, "go").await;
        let result = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::ToolResult { name, output, .. } if name == "delegate" => Some(output),
                _ => None,
            })
            .expect("the delegation was never answered");
        assert_eq!(result.content, "from elsewhere");
    }

    /// A line that opens on an agent's name is for that agent, unless a file of the project has the
    /// name.
    #[tokio::test]
    async fn a_line_opening_on_an_agent_is_for_it() {
        let h = Harness::new(Answer::Yes);
        let backend = fake_backend(vec![]).await;
        let mut reg = crate::boot::build_registry(h.root.clone()).0;
        reg.register(delegation(&backend.url, reg.read_only(&[]), 4));
        let agent = agent_with(&h, &backend, ToolProtocol::Native, reg);
        assert_eq!(
            agent.agent_named("@explore where is main"),
            Some(("explore", "where is main"))
        );
        assert_eq!(agent.agent_named("@nobody where is main"), None);
        assert_eq!(agent.agent_named("look at @explore"), None);
        h.write("explore", "a file");
        assert_eq!(agent.agent_named("@explore where is main"), None);
    }

    /// A plan approved ends planning: the steps become the task list and the turn goes on to change
    /// things.
    /// A plan refused leaves the session planning.
    #[tokio::test]
    async fn an_approved_plan_is_acted_on_and_a_refused_one_is_not() {
        for approve in [true, false] {
            let h = Harness::new(if approve { Answer::Yes } else { Answer::No });
            let backend = fake_backend(vec![
                sse(&[
                    r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"submit_plan","arguments":"{\"summary\": \"make b\", \"steps\": [\"write b.txt\"]}"}}]}}]}"#,
                    r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
                ]),
                sse(&[
                    r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c2","function":{"name":"write_file","arguments":"{\"path\": \"b.txt\", \"content\": \"x\"}"}}]}}]}"#,
                    r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
                ]),
                sse(&[
                    r#"{"choices":[{"delta":{"content":"done"}}]}"#,
                    r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
                ]),
            ])
            .await;
            let tasks = Arc::new(crate::tools::plan::TaskList::default());
            let reg = crate::boot::build_registry_with(
                h.root.clone(),
                tasks.clone(),
                crate::lsp::Languages::default(),
            )
            .0
             .0;
            let mut agent = agent_with(&h, &backend, ToolProtocol::Native, reg);
            agent.set_mode(Mode::Plan);
            drive(&mut agent, "make b").await;
            assert_eq!(h.path("b.txt").exists(), approve, "approve: {approve}");
            assert_eq!(agent.cfg.mode == Mode::Act, approve);
            assert_eq!(tasks.get().await.len(), usize::from(approve));
        }
    }

    /// A turn-end hook that blocks hands its reason to the model, and the turn goes on until the
    /// hook lets it end.
    #[tokio::test]
    async fn a_turn_end_hook_keeps_the_turn_going_once() {
        let h = Harness::new(Answer::Yes);
        let backend = fake_backend(vec![
            sse(&[
                r#"{"choices":[{"delta":{"content":"done"}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ]),
            sse(&[
                r#"{"choices":[{"delta":{"content":"really done"}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ]),
        ])
        .await;
        let reg = crate::boot::build_registry(h.root.clone()).0;
        let mut agent = agent_with(&h, &backend, ToolProtocol::Native, reg);
        agent.cfg.hooks.turn_end = vec![crate::config::Hook {
            command: "test -f held || { touch held; echo the tests fail >&2; exit 2; }".into(),
            matches: None,
            timeout: None,
        }];
        let events = drive(&mut agent, "fix it").await;
        let answers: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::AssistantText(s) if !s.is_empty() => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(answers, ["done", "really done"]);
        assert!(agent
            .session
            .history
            .iter()
            .any(|m| m.content.contains("the tests fail")));
    }

    /// An agent sent to work in the background lets the turn go on at once, and its answer is
    /// handed to the session when it comes.
    /// One that may change things is not sent.
    #[tokio::test]
    async fn an_agent_in_the_background_answers_later() {
        let h = Harness::new(Answer::Yes);
        let here = fake_backend(vec![
            sse(&[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"delegate","arguments":"{\"task\": \"find main\", \"background\": true}"}}]}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ]),
            sse(&[
                r#"{"choices":[{"delta":{"content":"started it"}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ]),
        ])
        .await;
        let there = fake_backend(vec![sse(&[
            r#"{"choices":[{"delta":{"content":"main is in src/main.rs"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        ])])
        .await;
        let (answers, mut answered) = tokio::sync::mpsc::channel(4);
        let mut reg = crate::boot::build_registry(h.root.clone()).0;
        reg.register(crate::tools::delegate::Delegate::new(Arc::new(
            crate::tools::delegate::Delegation {
                backend: Arc::new(crate::backend::client::BackendClient::new(
                    there.url.clone(),
                )),
                backends: Default::default(),
                registry: Arc::new(reg.clone()),
                agents: vec![crate::agent::agents::AgentSpec::explore()],
                search_model: "test".into(),
                edit_model: "test".into(),
                roles: Default::default(),
                models: Default::default(),
                context_tokens: 8192,
                max_answer_tokens: 4096,
                protocol: Default::default(),
                grammar_enabled: false,
                max_steps: 4,
                slots: Arc::new(tokio::sync::Semaphore::new(2)),
                parallel: 2,
                hooks: Default::default(),
                lsp: None,
                answers: Some(answers),
                depth: 1,
                works: Default::default(),
            },
        )));
        let mut agent = agent_with(&h, &here, ToolProtocol::Native, reg);
        let events = drive(&mut agent, "go").await;
        let result = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::ToolResult { name, output, .. } if name == "delegate" => Some(output),
                _ => None,
            })
            .expect("the delegation was never answered");
        assert!(
            result.content.contains("in the background"),
            "{}",
            result.content
        );
        let later = tokio::time::timeout(std::time::Duration::from_secs(10), answered.recv())
            .await
            .expect("no answer came")
            .unwrap();
        assert!(later.contains("main is in src/main.rs"), "{later}");
    }

    /// With a depth of two, an agent that names delegate hands work on to another, and the answer
    /// comes back up through it.
    #[tokio::test]
    async fn an_agent_hands_work_on_within_the_depth() {
        let h = Harness::new(Answer::Yes);
        let backend = fake_backend(vec![
            sse(&[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"delegate","arguments":"{\"agent\": \"lead\", \"task\": \"find it\"}"}}]}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ]),
            sse(&[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"l1","function":{"name":"delegate","arguments":"{\"task\": \"look deep\"}"}}]}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ]),
            sse(&[
                r#"{"choices":[{"delta":{"content":"deep down"}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ]),
            sse(&[
                r#"{"choices":[{"delta":{"content":"the lead says deep down"}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ]),
            sse(&[
                r#"{"choices":[{"delta":{"content":"done"}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ]),
        ])
        .await;
        let mut reg = crate::boot::build_registry(h.root.clone()).0;
        let lead = crate::agent::agents::parse(
            "lead",
            "---\ntools: read_file, delegate\n---\nHand the search on.",
            "test",
        )
        .unwrap();
        reg.register(crate::tools::delegate::Delegate::new(Arc::new(
            crate::tools::delegate::Delegation {
                backend: Arc::new(crate::backend::client::BackendClient::new(
                    backend.url.clone(),
                )),
                backends: Default::default(),
                registry: Arc::new(reg.clone()),
                agents: vec![crate::agent::agents::AgentSpec::explore(), lead],
                search_model: "test".into(),
                edit_model: "test".into(),
                roles: Default::default(),
                models: Default::default(),
                context_tokens: 8192,
                max_answer_tokens: 4096,
                protocol: Default::default(),
                grammar_enabled: false,
                max_steps: 4,
                slots: Arc::new(tokio::sync::Semaphore::new(1)),
                parallel: 1,
                hooks: Default::default(),
                lsp: None,
                answers: None,
                depth: 2,
                works: Default::default(),
            },
        )));
        let mut agent = agent_with(&h, &backend, ToolProtocol::Native, reg);
        let events =
            tokio::time::timeout(std::time::Duration::from_secs(20), drive(&mut agent, "go"))
                .await
                .expect("the agents waited on each other");
        let result = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::ToolResult { name, output, .. } if name == "delegate" => Some(output),
                _ => None,
            })
            .expect("the delegation was never answered");
        assert_eq!(result.content, "the lead says deep down");
    }

    /// A delegated session that runs out of steps says so, rather than handing back what it was
    /// about to do as its answer.
    #[tokio::test]
    async fn a_delegated_session_out_of_steps_says_it_did_not_finish() {
        let h = Harness::new(Answer::Yes);
        h.write("a.txt", "forty two");
        let backend = fake_backend(vec![
            sse(&[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"delegate","arguments":"{\"task\": \"what is in a.txt\"}"}}]}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ]),
            sse(&[
                r#"{"choices":[{"delta":{"content":"let me look at a.txt"}}]}"#,
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"s1","function":{"name":"read_file","arguments":"{\"path\": \"a.txt\"}"}}]}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ]),
            sse(&[
                r#"{"choices":[{"delta":{"content":"done"}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ]),
        ])
        .await;
        let mut reg = crate::boot::build_registry(h.root.clone()).0;
        reg.register(delegation(&backend.url, reg.read_only(&[]), 1));
        let mut agent = agent_with(&h, &backend, ToolProtocol::Native, reg);
        let events = drive(&mut agent, "go").await;
        let result = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::ToolResult { name, output, .. } if name == "delegate" => Some(output),
                _ => None,
            })
            .expect("the delegation was never answered");
        assert!(
            result.content.starts_with("It did not finish"),
            "{}",
            result.content
        );
        assert!(result.content.contains("let me look"), "{}", result.content);
    }

    #[test]
    fn a_tool_name_inside_a_longer_word_is_not_a_call() {
        assert!(names_word("I will read_file('a.rs')", "read_file"));
        assert!(names_word("run: read_file src/a.rs", "read_file"));
        assert!(!names_word("see read_files_doc for more", "read_file"));
        assert!(!names_word("the xread_file helper", "read_file"));
    }

    /// The same read twice in one run is one read.
    /// The model gets 2 answers, because 2 calls need 2 results, and the machine does the work
    /// once.
    #[tokio::test]
    async fn the_same_read_twice_in_one_run_is_read_once() {
        let h = Harness::new(Answer::Yes);
        h.write("a.txt", "forty two");
        let backend = fake_backend(vec![
            sse(&[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"read_file","arguments":"{\"path\": \"a.txt\"}"}},{"index":1,"id":"c2","function":{"name":"read_file","arguments":"{\"path\": \"a.txt\"}"}}]}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ]),
            sse(&[
                r#"{"choices":[{"delta":{"content":"forty two"}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ]),
        ])
        .await;
        let mut agent = agent_with_all_tools(&h, &backend, ToolProtocol::Native);
        let events = drive(&mut agent, "read a.txt twice").await;

        let results: Vec<&ToolOutput> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolResult { name, output, .. } if name == "read_file" => Some(output),
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), 2, "both calls need an answer");
        assert!(results[0].content.contains("forty two"), "{results:?}");
        assert!(results[1].content.contains("forty two"), "{results:?}");
        // The second is answered from the first, and says so.
        assert!(
            results[1].content.contains("you already ran this"),
            "{results:?}"
        );
    }

    /// A write makes every earlier read stale, so the same read after it runs again.
    #[tokio::test]
    async fn a_read_after_a_write_is_read_again() {
        let h = Harness::new(Answer::Yes);
        h.write("a.txt", "one");
        let backend = fake_backend(vec![
            sse(&[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"read_file","arguments":"{\"path\": \"a.txt\"}"}}]}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ]),
            sse(&[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c2","function":{"name":"write_file","arguments":"{\"path\": \"a.txt\", \"content\": \"two\"}"}}]}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ]),
            sse(&[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c3","function":{"name":"read_file","arguments":"{\"path\": \"a.txt\"}"}}]}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ]),
            sse(&[
                r#"{"choices":[{"delta":{"content":"it says two"}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ]),
        ])
        .await;
        let mut agent = agent_with_all_tools(&h, &backend, ToolProtocol::Native);
        let events = drive(&mut agent, "read, write, read").await;

        let last = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolResult { name, output, .. } if name == "read_file" => Some(output),
                _ => None,
            })
            .next_back()
            .expect("the file was never read back");
        assert!(last.content.contains("two"), "{last:?}");
        assert!(!last.content.contains("you already ran this"), "{last:?}");
    }

    /// A model that corrupts its own arguments alternates between calls that work and calls that
    /// cannot, so it never gets nowhere 3 steps running.
    /// What it never does is change anything.
    #[tokio::test]
    async fn a_turn_that_keeps_failing_and_changes_nothing_is_given_back() {
        let h = Harness::new(Answer::Yes);
        let mut answers = Vec::new();
        for i in 0..8 {
            h.write(&format!("f{i}.txt"), "one");
            // Each miss and each hit is a call of its own, so nothing is a repeat.
            for path in [format!("nope{i}.rs"), format!("f{i}.txt")] {
                answers.push(sse(&[
                    &format!(
                        r#"{{"choices":[{{"delta":{{"tool_calls":[{{"index":0,"id":"c{i}","function":{{"name":"read_file","arguments":"{{\"path\": \"{path}\"}}"}}}}]}}}}]}}"#
                    ),
                    r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
                ]));
            }
        }
        let backend = fake_backend(answers).await;
        let mut agent = agent_with_all_tools(&h, &backend, ToolProtocol::Native);
        agent.cfg.max_steps = 20;
        let events = drive(&mut agent, "find it").await;

        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::Notice(n)
                if n.contains("nothing has changed"))),
            "the turn ran to the step limit"
        );
    }

    #[test]
    fn a_bracketed_failure_is_not_an_answer() {
        assert!(is_backend_failure(
            "[Error: STREAMING PREFILL: Forward failed: cannot broadcast]"
        ));
        assert!(!is_backend_failure(
            "The error you are seeing comes from the parser."
        ));
    }

    /// An invented tool name is a paraphrase of a real one, and a refusal that says only that the
    /// name is wrong costs a step to guess again, usually to another invention.
    #[test]
    fn an_invented_tool_name_is_answered_with_the_real_one() {
        let mut reg = Registry::new();
        reg.register(ListDir);
        reg.register(crate::tools::fs_read::ReadFile);
        assert_eq!(
            nearest_tool(&reg, "list_files").as_deref(),
            Some("list_dir")
        );
        assert_eq!(
            nearest_tool(&reg, "read_the_file").as_deref(),
            Some("read_file")
        );
        assert_eq!(nearest_tool(&reg, "zzz").as_deref(), None);
    }

    /// A change is read by a second model before it is written.
    /// The value is in the disagreement, so the objection stops the write and reaches the one who
    /// proposed it.
    #[tokio::test]
    async fn a_second_reading_stops_a_change_it_objects_to() {
        let h = Harness::new(Answer::Yes);
        h.write("a.txt", "keep me");
        let backend = fake_backend(vec![
            sse(&[
                r#"{"choices":[{"delta":{"content":"replacing it"}}]}"#,
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"write_file","arguments":"{\"path\": \"a.txt\", \"content\": \"gone\"}"}}]}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ]),
            // The reviewer, answering the one question it was asked.
            json_reply("it deletes the line the user asked to keep"),
            sse(&[
                r#"{"choices":[{"delta":{"content":"understood"}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ]),
        ])
        .await;
        let mut agent = agent_with_all_tools(&h, &backend, ToolProtocol::Native);
        agent.cfg.roles.review = Some("small".into());
        let events = drive(&mut agent, "add a line to a.txt and keep what is there").await;

        let result = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::ToolResult { name, output, .. } if name == "write_file" => Some(output),
                _ => None,
            })
            .expect("the call was never answered");
        assert!(result.is_error, "{result:?}");
        assert!(result.content.contains("second reading"), "{result:?}");
        // The file is untouched: the objection came before the write, not after it.
        assert_eq!(h.read("a.txt"), "keep me");
    }

    /// A reviewer that agrees says so in one word and gets out of the way.
    #[tokio::test]
    async fn a_second_reading_that_agrees_changes_nothing() {
        let h = Harness::new(Answer::Yes);
        h.write("a.txt", "one");
        let backend = fake_backend(vec![
            sse(&[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"write_file","arguments":"{\"path\": \"a.txt\", \"content\": \"two\"}"}}]}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ]),
            json_reply("OK"),
            sse(&[
                r#"{"choices":[{"delta":{"content":"done"}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ]),
        ])
        .await;
        let mut agent = agent_with_all_tools(&h, &backend, ToolProtocol::Native);
        agent.cfg.roles.review = Some("small".into());
        let events = drive(&mut agent, "put two in a.txt").await;

        let result = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::ToolResult { name, output, .. } if name == "write_file" => Some(output),
                _ => None,
            })
            .expect("the call was never answered");
        assert!(!result.is_error, "{result:?}");
        assert_eq!(h.read("a.txt"), "two");
    }

    /// A project fails the same way twice.
    /// The complaint is held until a change makes it go away, and the next time it appears it
    /// arrives with what worked.
    #[tokio::test]
    async fn a_complaint_comes_back_with_what_fixed_it() {
        let h = Harness::new(Answer::Yes);
        h.write("a.txt", "one");
        let mut agent = {
            let backend = fake_backend(vec![sse(&[
                r#"{"choices":[{"delta":{"content":"done"}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ])])
            .await;
            agent_with_all_tools(&h, &backend, ToolProtocol::Native)
        };
        agent
            .repairs
            .failed("src/a.rs:12:5: cannot find value `foo` in this scope");
        agent
            .repairs
            .fixed("edited src/a.rs\nadded the missing use");

        assert_eq!(
            agent
                .repairs
                .recall("src/other.rs:88:1: cannot find value `foo` in this scope"),
            Some("edited src/a.rs")
        );
        assert!(agent.explain_choice().contains("edited src/a.rs"));
    }

    /// A small model guesses the read that is coming and it is made while the answer is still being
    /// written, so the step that asks for it is answered without a round trip.
    #[tokio::test]
    async fn a_guessed_read_is_ready_before_it_is_asked_for() {
        let h = Harness::new(Answer::Yes);
        h.write("a.txt", "forty two");
        let backend = fake_backend(vec![json_reply(
            r#"{"name": "read_file", "arguments": {"path": "a.txt"}}"#,
        )])
        .await;
        let mut agent = agent_with_all_tools(&h, &backend, ToolProtocol::Native);
        agent.session.push(Message::user("what does a.txt say"));

        agent.cfg.guess_with = Some("test".into());
        let (handle, _stop) = agent
            .start_guess(&CancellationToken::new())
            .expect("speculation is on");
        let (key, out) = handle.await.unwrap().expect("the guess was not made");
        assert!(out.content.contains("forty two"), "{out:?}");
        // Filed where a repeat of the same call would find it.
        assert!(key.contains("read_file"), "{key}");
        assert!(key.contains("a.txt"), "{key}");
    }

    /// A guess that would change something is dropped rather than run: being wrong has to stay
    /// free.
    #[tokio::test]
    async fn a_guess_that_would_write_is_not_made() {
        let h = Harness::new(Answer::Yes);
        h.write("a.txt", "one");
        let backend = fake_backend(vec![json_reply(
            r#"{"name": "write_file", "arguments": {"path": "a.txt", "content": "two"}}"#,
        )])
        .await;
        let mut agent = agent_with_all_tools(&h, &backend, ToolProtocol::Native);
        agent.session.push(Message::user("say hello"));

        agent.cfg.guess_with = Some("test".into());
        let (handle, _stop) = agent
            .start_guess(&CancellationToken::new())
            .expect("speculation is on");
        assert!(handle.await.unwrap().is_none());
        assert_eq!(h.read("a.txt"), "one");
    }

    /// A guess for a tool this session does not have is not a guess.
    #[tokio::test]
    async fn a_guess_at_a_tool_that_does_not_exist_is_dropped() {
        let h = Harness::new(Answer::Yes);
        let backend = fake_backend(vec![json_reply(
            r#"{"name": "list_everything", "arguments": {}}"#,
        )])
        .await;
        let mut agent = agent_with_all_tools(&h, &backend, ToolProtocol::Native);
        agent.session.push(Message::user("have a look"));
        agent.cfg.guess_with = Some("test".into());
        let (handle, _stop) = agent
            .start_guess(&CancellationToken::new())
            .expect("speculation is on");
        assert!(handle.await.unwrap().is_none());
    }

    /// A model handed what it was about to ask for is not repeating itself, and being told that it
    /// is would be a lie about its own turn.
    /// The second ask is a repeat.
    #[tokio::test]
    async fn an_answer_that_came_from_a_guess_is_served_plainly() {
        let h = Harness::new(Answer::Yes);
        h.write("a.txt", "forty two");
        let backend = fake_backend(vec![
            sse(&[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"read_file","arguments":"{\"path\": \"a.txt\"}"}}]}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ]),
            sse(&[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c2","function":{"name":"read_file","arguments":"{\"path\": \"a.txt\"}"}}]}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ]),
            sse(&[
                r#"{"choices":[{"delta":{"content":"forty two"}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ]),
        ])
        .await;
        let mut agent = agent_with_all_tools(&h, &backend, ToolProtocol::Native);
        // As if a guess had already fetched it.
        let key = repeat_key(&ResolvedCall {
            id: String::new(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path": "a.txt"}),
        });
        agent
            .repeats
            .insert(key.clone(), ToolOutput::ok("    1 | forty two"));
        agent.from_guess.insert(key);

        let events = drive(&mut agent, "what does a.txt say").await;
        let results: Vec<&ToolOutput> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolResult { name, output, .. } if name == "read_file" => Some(output),
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), 2, "both asks need an answer");
        assert!(
            !results[0].content.contains("you already ran this"),
            "{results:?}"
        );
        assert!(results[0].content.contains("forty two"), "{results:?}");
        // The second ask is the model repeating itself, and is told so.
        assert!(
            results[1].content.contains("you already ran this"),
            "{results:?}"
        );
    }

    /// A server that does more than text lists what it does, and none of it can answer a question.
    /// Offering it to somebody choosing a model is offering a choice that cannot work.
    #[test]
    fn the_catalogue_offers_only_what_can_answer() {
        for name in [
            "Systran/faster-whisper-base",
            "coqui/XTTS-v2",
            "black-forest-labs/FLUX.1-schnell",
            "Qwen/Qwen-Image",
            "openai/clip-vit-large-patch14",
            "llama-guard3:1b",
        ] {
            assert!(!can_hold_a_conversation(name), "{name}");
        }
        // A name that says nothing either way is kept.
        for name in ["qwen3-coder:30b", "mistral-small3.2:latest", "boogu"] {
            assert!(can_hold_a_conversation(name), "{name}");
        }
    }

    /// A question about the code, answered from memory, is sent back to be answered from the code.
    #[tokio::test]
    async fn a_question_about_the_code_answered_blind_is_sent_back() {
        let h = Harness::new(Answer::Yes);
        h.write("src/lib.rs", "pub fn strength() {}");
        let backend = fake_backend(vec![
            // Answers straight away, having read nothing.
            sse(&[
                r#"{"choices":[{"delta":{"content":"it is a Rust crate for brewing"}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ]),
            // Told to look, it looks.
            sse(&[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"read_file","arguments":"{\"path\": \"src/lib.rs\"}"}}]}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ]),
            sse(&[
                r#"{"choices":[{"delta":{"content":"it has one function, strength"}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ]),
        ])
        .await;
        let mut agent = agent_with_all_tools(&h, &backend, ToolProtocol::Native);
        let events = drive(&mut agent, "based on the sources what does this project do").await;

        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolResult { name, .. }
                if name == "read_file")),
            "it was never sent back to look"
        );
        let last = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::AssistantText(t) => Some(t.as_str()),
                _ => None,
            })
            .next_back();
        assert_eq!(last, Some("it has one function, strength"));
    }

    /// Only where the person said the project.
    /// A question about Rust is answered from what the model knows, and sending it to read the tree
    /// would be beside the point.
    #[test]
    fn a_question_that_is_not_about_here_is_left_alone() {
        assert!(asks_about_the_project(
            "based on the sources, what does it do"
        ));
        assert!(asks_about_the_project(
            "what are the features of this project"
        ));
        assert!(!asks_about_the_project("what does the borrow checker do"));
        assert!(!asks_about_the_project("write me a haiku"));
    }

    /// A model asking the same question every step learns nothing each time.
    /// The memo saves the work.
    /// Only stopping saves the round trip.
    #[tokio::test]
    async fn a_turn_that_only_repeats_itself_is_given_back() {
        let h = Harness::new(Answer::Yes);
        h.write("a.txt", "one");
        let again = || {
            sse(&[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"read_file","arguments":"{\"path\": \"a.txt\"}"}}]}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ])
        };
        let backend =
            fake_backend(vec![again(), again(), again(), again(), again(), again()]).await;
        let mut agent = agent_with_all_tools(&h, &backend, ToolProtocol::Native);
        agent.cfg.max_steps = 16;
        let events = drive(&mut agent, "read a.txt").await;

        let reads = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ToolResult { name, .. } if name == "read_file"))
            .count();
        // The first read, then 3 that got nowhere.
        assert_eq!(reads, 4, "the turn kept asking");
        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::Notice(n)
                if n.contains("got nowhere"))),
            "the user was not told why it stopped"
        );
    }

    /// A model repeating a broken idea costs a full context read per step. 3 steps where nothing
    /// worked is enough to hand the turn back.
    #[tokio::test]
    async fn a_turn_where_nothing_works_is_given_back() {
        let h = Harness::new(Answer::Yes);
        let miss = || {
            sse(&[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"read_file","arguments":"{\"path\": \"nope.rs\"}"}}]}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ])
        };
        let backend = fake_backend(vec![miss(), miss(), miss(), miss(), miss()]).await;
        let mut agent = agent_with_all_tools(&h, &backend, ToolProtocol::Native);
        let events = drive(&mut agent, "read nope.rs").await;

        let calls = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ToolResult { .. }))
            .count();
        assert_eq!(calls, 3, "the turn ran past the point of trying");
        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::Notice(n)
                if n.contains("got nowhere"))),
            "the user was not told why it stopped"
        );
    }

    /// A backend that takes the tool definitions and drops them fails nothing: the model writes the
    /// call it wanted to make into its answer.
    /// The session notices and asks again under the protocol that needs no cooperation from the
    /// server.
    #[tokio::test]
    async fn a_backend_that_ignores_the_tools_is_noticed_and_worked_around() {
        let h = Harness::new(Answer::Yes);
        h.write("a.txt", "forty two");
        let backend = fake_backend(vec![
            // Tools were sent and dropped: the model writes the call out instead.
            sse(&[
                r#"{"choices":[{"delta":{"content":"I will read it: read_file('a.txt')"}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ]),
            // Asked again, under the envelope.
            sse(&[
                r#"{"choices":[{"delta":{"content":"{\"assistant_text\":\"\",\"tool_calls\":[{\"name\":\"read_file\",\"arguments\":{\"path\":\"a.txt\"}}]}"}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ]),
            sse(&[
                r#"{"choices":[{"delta":{"content":"{\"assistant_text\":\"it says forty two\",\"tool_calls\":[]}"}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ]),
        ])
        .await;
        let mut agent = agent_with_all_tools(&h, &backend, ToolProtocol::Native);
        agent.cfg.allow_fallback = true;
        let events = drive(&mut agent, "what does a.txt say").await;

        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::Notice(n)
                if n.contains("ignored them"))),
            "the user was never told the tools were dropped"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolResult { name, .. }
                if name == "read_file")),
            "the file was never read"
        );
        assert_eq!(agent.cfg.protocol, ToolProtocol::Envelope);
    }

    /// A sub-session says as much as any other session, and a long answer says more than the events
    /// channel holds.
    /// Its caller has to read while it runs.
    #[tokio::test]
    async fn a_long_delegated_answer_still_comes_back() {
        let h = Harness::new(Answer::Yes);
        // Well past the channel's depth, so a caller that reads only at the end stops here.
        let long: Vec<String> = (0..200)
            .map(|i| format!(r#"{{"choices":[{{"delta":{{"content":"word{i} "}}}}]}}"#))
            .collect();
        let mut sub_answer: Vec<&str> = long.iter().map(String::as_str).collect();
        sub_answer.push(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#);
        let backend = fake_backend(vec![
            sse(&[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"delegate","arguments":"{\"task\": \"say a lot\"}"}}]}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ]),
            sse(&sub_answer),
            sse(&[
                r#"{"choices":[{"delta":{"content":"done"}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ]),
        ])
        .await;

        let mut reg = crate::boot::build_registry(h.root.clone()).0;
        let sub = Arc::new(reg.clone());
        reg.register(delegation(&backend.url, (*sub).clone(), 4));
        let mut agent = agent_with(&h, &backend, ToolProtocol::Native, reg);
        let events = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            drive(&mut agent, "say a lot"),
        )
        .await
        .expect("the delegated turn never returned");

        let result = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::ToolResult { name, output, .. } if name == "delegate" => Some(output),
                _ => None,
            })
            .expect("the delegation was never answered");
        assert!(!result.is_error, "{result:?}");
        assert!(result.content.contains("word199"), "{}", result.content);
    }

    /// A pre-tool hook that refuses stops the call, and what it printed is what the model is told:
    /// guidance an agent cannot skip, which is the only kind that holds.
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
        agent.cfg.hooks.pre_tool = vec![crate::config::Hook {
            command: "echo the tree is not clean >&2; exit 1".into(),
            matches: None,
            timeout: None,
        }];
        let events = drive(&mut agent, "change it").await;

        let refusal = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::ToolResult { name, output, .. } if name == "write_file" => Some(output),
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

    /// A hook at the end of a turn is for the person: it says what it did, and it runs even for a
    /// turn that was abandoned, because that turn can have changed files.
    #[tokio::test]
    async fn the_turn_end_hook_reports_to_the_user() {
        let h = Harness::new(Answer::Yes);
        let backend = fake_backend(vec![sse(&[
            r#"{"choices":[{"delta":{"content":"nothing to do"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        ])])
        .await;
        let mut agent = agent_for(&h, &backend, ToolProtocol::Native);
        agent.cfg.hooks.turn_end = vec![crate::config::Hook {
            command: "echo formatted 3 files".into(),
            matches: None,
            timeout: None,
        }];
        let events = drive(&mut agent, "hello").await;
        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentEvent::Notice(s) if s.contains("formatted 3 files")
            )),
            "the hook said nothing: {events:?}"
        );
    }

    /// While planning, a tool that would change something is refused before it runs, and the
    /// refusal says why so the model writes a plan instead of calling again.
    /// A tool that only reads still runs: a plan nobody could research would be worth nothing.
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
                AgentEvent::ToolResult { name, output, .. } if name == "write_file" => Some(output),
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
                AgentEvent::ToolResult { name, output, .. } if name == "read_file" && !output.is_error
            )),
            "reading was refused too: {events:?}"
        );
        // The model is told it is planning, so it stops calling and writes the plan.
        assert!(agent.session.history[0]
            .content
            .contains("You are PLANNING"));
    }

    /// A switch takes the window the configuration gives that model, and a name the backend does
    /// not serve is refused rather than carried into every later request.
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

        // The fake backend answers nothing on /v1/models, so a name is taken on trust: a backend
        // that cannot list is not a backend that cannot serve.
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

    /// A line typed while the turn is running reaches the model on the next step, and says so.
    /// Without this the only interventions are abandoning the turn or waiting for it to end, which
    /// is restarting rather than steering.
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
        // Queued before the turn is driven: the drain is at the step boundary, so it is read on the
        // step it precedes rather than after the turn.
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

    /// The native path end to end: a streamed call is reassembled, dispatched, and answered against
    /// the id the server issued.
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

        // The assistant turn carries the call.
        // The result answers that id.
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

    /// The tools have to actually reach the server, or the model is being asked to call things it
    /// has not been told about.
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

        // The preamble has to change with the protocol, or the model is reading the contract for
        // the other one.
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

    /// A write is a boundary on both sides: what it changes is what the next read would see, and
    /// the checkpoint after it has to describe that call alone.
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

    /// 2 writes must not be grouped, or one checkpoint would cover both.
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

    /// A tool that does not exist is answered with a refusal, and grouping it would only obscure
    /// which call was refused.
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

    /// Reads that run together still answer in the order they were asked, whatever order they
    /// finish in.
    /// A result is matched to its call by position and by id.
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

    /// The whole point: the model is raised because a compiler said the change was wrong, not
    /// because anything guessed that the task looked hard.
    #[tokio::test]
    async fn a_failed_check_raises_the_model_and_two_clean_steps_lower_it() {
        let (mut agent, _rx) = agent_at("small").await;
        let (tx, _keep) = mpsc::channel(16);

        agent.reconsider_model(&step(Some(true)), &tx).await;
        assert_eq!(agent.cfg.model, "small", "a clean step raised the model");

        agent.reconsider_model(&step(Some(false)), &tx).await;
        assert_eq!(agent.cfg.model, "big");
        assert_eq!(agent.escalated_from.as_deref(), Some("small"));

        // One clean step is not enough: the next thing it writes may be the rest of the same
        // mistake.
        agent.reconsider_model(&step(Some(true)), &tx).await;
        assert_eq!(agent.cfg.model, "big");
        agent.reconsider_model(&step(Some(true)), &tx).await;
        assert_eq!(agent.cfg.model, "small");
        assert!(agent.escalated_from.is_none());
    }

    /// A mode that borrows a model gives it back.
    /// Without this a session that planned once would finish the day on the planning model, which
    /// is the expensive one.
    #[tokio::test]
    async fn leaving_planning_gives_back_the_model_it_borrowed() {
        let h = Harness::new(Answer::Yes);
        let backend = fake_backend(vec![]).await;
        let mut agent = agent_for(&h, &backend, ToolProtocol::Native);
        agent.cfg.model = "working".into();
        agent.cfg.roles.plan = Some("thinking".into());

        agent.set_mode(Mode::Plan);
        assert_eq!(agent.cfg.model, "thinking");
        agent.set_mode(Mode::Act);
        assert_eq!(agent.cfg.model, "working");
    }

    /// What the project taught is applied once, at the start of a turn, so a session does not pay
    /// again for the bad step the last one already paid for.
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
        // What it was raised from is remembered, so 2 clean writes bring it back.
        assert_eq!(agent.escalated_from.as_deref(), Some("small"));
    }

    /// One rung.
    /// A second failure while already raised does not look for a third model, because nothing here
    /// knows whether a third would help.
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

    /// A step that changed nothing says nothing about the model, so it neither raises nor counts
    /// towards coming back down.
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

    /// The rate is the answer's tokens over the time spent waiting for the model, not over the
    /// whole turn: time in a tool is not the model being slow.
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
