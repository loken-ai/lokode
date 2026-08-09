use std::sync::Arc;

use anyhow::Result;
use futures::StreamExt;
use jsonschema::Validator;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::protocol::TurnEnvelope;
use crate::agent::session::Session;
use crate::backend::client::{BackendClient, StreamEvent};
use crate::backend::wire::Usage;
use crate::backend::repair::{self, MAX_REPAIR_ATTEMPTS};
use crate::backend::schema::{build_turn_schema, tool_manifest};
use crate::backend::wire::{ChatCompletionRequest, Message, ResponseFormat};
use crate::tools::registry::{Registry, ToolCtx, ToolOutput};

/// Events the agent loop emits so the UI can render progress.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    AssistantDelta(String),
    AssistantText(String),
    ToolCall { name: String, args_pretty: String },
    ToolResult { name: String, output: ToolOutput },
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

/// One model response, with whatever the server said about how it ended.
#[derive(Default)]
struct Completion {
    text: String,
    /// `"length"` means the answer was cut off rather than finished.
    finish_reason: Option<String>,
    usage: Option<Usage>,
}

pub struct AgentConfig {
    pub model: String,
    pub grammar_enabled: bool,
    pub max_steps: usize,
    pub max_answer_tokens: usize,
}

pub struct Agent {
    pub backend: BackendClient,
    pub registry: Arc<Registry>,
    pub session: Session,
    pub cfg: AgentConfig,
    pub ctx: ToolCtx,
}

impl Agent {
    pub fn build_system_prompt(reg: &Registry) -> String {
        format!(
            "You are a local-first code agent driving a TUI on the user's machine.\n\
             Respond with a single JSON object matching the schema:\n\
               {{ \"assistant_text\": \"<prose to user>\", \"tool_calls\": [ {{ \"name\": <tool>, \"arguments\": {{...}} }} ] }}\n\
             Emit zero tool_calls when the task is finished or when you only need to talk.\n\
             Tool results will be returned to you as role=tool messages on the next turn.\n\n\
             {}",
            tool_manifest(reg)
        )
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
                let _ = events
                    .send(AgentEvent::Notice(format!(
                        "context full: {dropped} earlier message(s) elided to keep the tool manifest in view"
                    )))
                    .await;
            }
            let sent_chars = self.session.size_chars();
            let completion = match self.stream_one(&turn_schema, &cancel, &events).await {
                Ok(c) => c,
                Err(_) if cancel.is_cancelled() => {
                    self.abandon(&events, "interrupted").await;
                    return Ok(());
                }
                Err(e) => {
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
            if completion.finish_reason.as_deref() == Some("length") {
                let _ = events
                    .send(AgentEvent::Notice(
                        "the answer hit the token limit and was cut off; \
                         raise --max-answer-tokens if this repeats"
                            .into(),
                    ))
                    .await;
            }

            let raw = completion.text;
            let envelope = match self
                .parse_with_repair(&raw, &turn_schema, &validator, &cancel)
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

            // Echo assistant_text to the UI
            if !envelope.assistant_text.is_empty() {
                let _ = events
                    .send(AgentEvent::AssistantText(envelope.assistant_text.clone()))
                    .await;
            }

            // Persist the JSON envelope in history (the model expects to see its own prior tool_calls).
            let assistant_serialised = serde_json::to_string(&serde_json::json!({
                "assistant_text": envelope.assistant_text,
                "tool_calls": envelope.tool_calls.iter().map(|tc| serde_json::json!({
                    "name": tc.name,
                    "arguments": tc.arguments,
                })).collect::<Vec<_>>(),
            }))?;
            self.session.push(Message::assistant(assistant_serialised));

            if envelope.tool_calls.is_empty() {
                let _ = events.send(AgentEvent::TurnDone).await;
                return Ok(());
            }

            for tc in envelope.tool_calls {
                // Every call the model just made needs a result, even the ones that will
                // never run: the assistant turn listing them is already in the history,
                // and a call with no answer behind it describes an action whose outcome
                // the model cannot see. Say it was interrupted and move on.
                if cancel.is_cancelled() {
                    self.session.push(Message::tool(
                        "[error] interrupted by user".to_string(),
                        uuid::Uuid::new_v4().to_string(),
                    ));
                    continue;
                }
                let args_pretty = serde_json::to_string_pretty(&tc.arguments).unwrap_or_default();
                let _ = events
                    .send(AgentEvent::ToolCall {
                        name: tc.name.clone(),
                        args_pretty,
                    })
                    .await;
                let tool_id = uuid::Uuid::new_v4().to_string();
                let result = self.dispatch_tool(&tc.name, tc.arguments).await;
                let _ = events
                    .send(AgentEvent::ToolResult {
                        name: tc.name.clone(),
                        output: result.clone(),
                    })
                    .await;
                let prefix = if result.is_error { "[error] " } else { "" };
                self.session
                    .push(Message::tool(format!("{prefix}{}", result.content), tool_id));
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

    async fn stream_one(
        &self,
        schema: &Value,
        cancel: &CancellationToken,
        events: &mpsc::Sender<AgentEvent>,
    ) -> Result<Completion> {
        let response_format = if self.cfg.grammar_enabled {
            Some(ResponseFormat::json_schema("turn", schema.clone()))
        } else {
            None
        };

        let req = ChatCompletionRequest {
            model: self.cfg.model.clone(),
            messages: self.session.history.clone(),
            stream: Some(true),
            temperature: Some(0.2),
            top_p: None,
            max_tokens: Some(self.cfg.max_answer_tokens as u32),
            seed: None,
            response_format,
            user: Some(self.session.id.clone()),
            stream_options: None,
        };

        let mut stream = tokio::select! {
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
                StreamEvent::End { finish_reason, usage } => {
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
            let prompt = repair::repair_prompt(&last_err.to_string(), &current);
            let repair_req = ChatCompletionRequest {
                model: self.cfg.model.clone(),
                messages: vec![
                    Message::system("You are a strict JSON formatter. Output a single JSON object only — no prose, no fences."),
                    Message::user(prompt),
                ],
                stream: Some(false),
                temperature: Some(0.0),
                top_p: None,
                max_tokens: Some(4096),
                seed: Some(0),
                response_format: Some(ResponseFormat::json_schema("turn", schema.clone())),
                user: Some(format!("{}-repair", self.session.id)),
                stream_options: None,
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
            return ToolOutput::err(format!("unknown tool '{name}'"));
        };
        match tool.run(&self.ctx, args).await {
            Ok(o) => o,
            Err(e) => ToolOutput::err(format!("tool '{name}' failed: {e:#}")),
        }
    }
}
