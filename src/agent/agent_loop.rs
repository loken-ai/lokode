use std::sync::Arc;

use anyhow::Result;
use futures::StreamExt;
use jsonschema::Validator;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::agent::protocol::TurnEnvelope;
use crate::agent::session::Session;
use crate::backend::client::BackendClient;
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

pub struct AgentConfig {
    pub model: String,
    pub grammar_enabled: bool,
    pub max_steps: usize,
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
    /// model emits zero tool_calls or `max_steps` is reached.
    pub async fn run_user_turn(
        &mut self,
        user_input: String,
        events: mpsc::Sender<AgentEvent>,
    ) -> Result<()> {
        self.session.push(Message::user(user_input));

        let turn_schema = build_turn_schema(&self.registry);
        let validator = Validator::new(&turn_schema).map_err(|e| anyhow::anyhow!("{e}"))?;

        for step in 0..self.cfg.max_steps {
            let raw = self.stream_one(&turn_schema, &events).await?;
            let envelope = match self.parse_with_repair(&raw, &turn_schema, &validator).await {
                Ok(v) => v,
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

    async fn stream_one(
        &self,
        schema: &Value,
        events: &mpsc::Sender<AgentEvent>,
    ) -> Result<String> {
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
            max_tokens: Some(4096),
            seed: None,
            response_format,
            user: Some(self.session.id.clone()),
        };

        let mut stream = self.backend.stream(req).await?;
        let mut accum = String::new();
        while let Some(piece) = stream.next().await {
            match piece {
                Ok(p) => {
                    accum.push_str(&p);
                    let _ = events.send(AgentEvent::AssistantDelta(p)).await;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(accum)
    }

    async fn parse_with_repair(
        &mut self,
        raw: &str,
        schema: &Value,
        validator: &Validator,
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
            };
            current = self.backend.complete(repair_req).await?;
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
