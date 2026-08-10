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

/// Events the agent loop emits so the UI can render progress.
#[derive(Debug, Clone)]
pub enum AgentEvent {
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
    TurnDone,
}

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

pub struct Agent {
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
    pub fn build_system_prompt(reg: &Registry, protocol: ToolProtocol) -> String {
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
        let preamble = Self::build_system_prompt(&self.registry, ToolProtocol::Envelope);
        self.session.set_system_prompt(preamble);
    }

    /// Run one user turn end-to-end. Loops model→tools→model until the
    /// model emits zero tool_calls, `max_steps` is reached, or `cancel` fires.
    ///
    /// `cancel` is created by the UI before the input is handed over, so the side that
    /// has to react to a keystroke already holds the handle it needs. It is stored on the
    /// tool context for the length of the turn, which is how a tool waiting on a
    /// subprocess learns that the user has stopped caring about the answer.
    pub async fn run_user_turn(
        &mut self,
        user_input: String,
        cancel: CancellationToken,
        events: mpsc::Sender<AgentEvent>,
    ) -> Result<()> {
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
            let mut attempt = self.stream_one(&turn_schema, &cancel, &events).await;

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
                    attempt = self.stream_one(&turn_schema, &cancel, &events).await;
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
            if let Some(usage) = completion.usage {
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
                .read_turn(completion, &turn_schema, &validator, &cancel)
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
                let _ = events.send(AgentEvent::TurnDone).await;
                return Ok(());
            }

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

                let results = futures::future::join_all(
                    run.iter()
                        .map(|call| self.dispatch_tool(&call.name, call.arguments.clone())),
                )
                .await;

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
                        self.record_change(&call.name, &events).await;
                    }
                }
            }
        }

        let _ = events
            .send(AgentEvent::Notice(format!(
                "stopped after {} steps (max_steps)",
                self.cfg.max_steps
            )))
            .await;
        let _ = events.send(AgentEvent::TurnDone).await;
        Ok(())
    }

    /// Close an unanswered turn.
    ///
    /// The user message is already in the history; without a reply behind it the next
    /// turn puts two `user` roles in a row, which is not a shape the compactor's
    /// boundary search expects.
    async fn abandon(&mut self, events: &mpsc::Sender<AgentEvent>, why: &str) {
        self.session.push(Message::assistant(TURN_ABANDONED));
        let _ = events.send(AgentEvent::Notice(why.to_string())).await;
        let _ = events.send(AgentEvent::TurnDone).await;
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
                    out.text.push_str(&p);
                    let _ = events.send(AgentEvent::AssistantDelta(p)).await;
                }
                // Shown as it arrives and then dropped. It is deliberately not appended to
                // `out.text`: the scratchpad is not the answer, and replaying it into the
                // history would teach the next turn that it was.
                StreamEvent::Reasoning(p) => {
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
                model: self.cfg.model.clone(),
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
        let started = std::time::Instant::now();
        let out = match tool.run(&self.ctx, args).await {
            Ok(o) => o,
            Err(e) => ToolOutput::err(format!("tool '{name}' failed: {e:#}")),
        };
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
                loop {
                    let Ok(n) = sock.read(&mut chunk).await else {
                        break;
                    };
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
        let registry = Arc::new(reg);
        Agent {
            backend: Arc::new(crate::backend::client::BackendClient::new(
                backend.url.clone(),
            )),
            session: Session::new(Agent::build_system_prompt(&registry, protocol), 8192, 4096),
            registry,
            cfg: AgentConfig {
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
        }
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
}
