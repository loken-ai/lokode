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
pub struct Delegation {
    pub backend: Arc<dyn crate::backend::provider::Provider>,
    pub registry: Arc<Registry>,
    /// The model a delegated turn uses: the search role when the project named one.
    pub model: String,
    pub roles: crate::agent::roles::Roles,
    pub context_tokens: usize,
    pub max_answer_tokens: usize,
    pub protocol: crate::agent::protocol::SharedProtocol,
    pub grammar_enabled: bool,
    /// Fewer than a full turn's: a delegated piece of work that needs twenty steps was
    /// not self-contained, and the caller should have been told rather than waited.
    pub max_steps: usize,
}

pub struct Delegate {
    pub what: Arc<Delegation>,
}

#[derive(Deserialize)]
struct Args {
    task: String,
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

    fn description(&self) -> &str {
        "Hand one self-contained question to a fresh session with its own context window, \
         and get back only its answer. Use it for a search that would read many files to \
         answer one question."
    }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"task": {"type": "string"}},
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
        // Its own history, its own budget, and the same tools: a sub-session that could
        // not read the project could not answer anything about it.
        let system = crate::agent::agent_loop::Agent::build_system_prompt(
            &w.registry,
            w.protocol.get(),
            crate::agent::agent_loop::Mode::Act,
            Some(
                "You are answering one question for another agent. Use the tools to find \
                 out, then answer in a few lines. Nothing you read is returned, only what \
                 you say, so state the answer rather than pointing at where it is.",
            ),
        );
        let mut sub = crate::agent::agent_loop::Agent {
            this_turn: Default::default(),
            repairs: Default::default(),
            background: None,
            checked_this_step: Default::default(),
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
            lsp: None,
            steer: tokio::sync::mpsc::channel(1).1,
            backend: w.backend.clone(),
            registry: w.registry.clone(),
            session: crate::agent::session::Session::new(
                system,
                w.context_tokens,
                w.max_answer_tokens,
            ),
            cfg: crate::agent::agent_loop::AgentConfig {
                model: w.model.clone(),
                // The sub-session is short and its model is already chosen; nothing after
                // it is known, so there is nothing to hold ready.
                preload: false,
                guess_with: None,
                roles: w.roles.clone(),
                mode: crate::agent::agent_loop::Mode::Act,
                hooks: Default::default(),
                instructions: None,
                remember_model_in: None,
                context_tokens: w.context_tokens,
                models: Default::default(),
                grammar_enabled: w.grammar_enabled,
                max_steps: w.max_steps,
                max_answer_tokens: w.max_answer_tokens,
                protocol: w.protocol.get(),
                allow_fallback: false,
                shared_protocol: None,
            },
            // The caller's context, so its approvals, its cancellation and its root are
            // the sub-session's too: this is the same user's work, delegated.
            ctx: ctx.clone(),
            checkpoints: None,
            turn: 0,
        };

        let cancel = ctx.cancel.clone();
        let events = match sub.collect_turn(a.task, cancel).await {
            Ok(events) => events,
            Err(e) => return Ok(ToolOutput::err(format!("the delegated turn failed: {e:#}"))),
        };
        let mut answer = String::new();
        let mut notices = Vec::new();
        for event in events {
            match event {
                crate::agent::agent_loop::AgentEvent::AssistantText(s) => {
                    if !s.trim().is_empty() {
                        answer = s;
                    }
                }
                crate::agent::agent_loop::AgentEvent::Notice(s) => notices.push(s),
                _ => {}
            }
        }
        if answer.trim().is_empty() {
            return Ok(ToolOutput::err(format!(
                "the delegated turn answered nothing{}",
                if notices.is_empty() {
                    String::new()
                } else {
                    format!(": {}", notices.join("; "))
                }
            )));
        }
        tracing::info!(
            steps = sub.spend.steps,
            answer_chars = answer.len(),
            "delegated turn finished"
        );
        Ok(ToolOutput::ok(ctx.clamp(answer)))
    }
}
