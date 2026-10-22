//! Finding out whether a backend really does native tool calls.
//!
//! A server that will not take the `tools` field says so, and the session drops to the
//! envelope protocol on the spot. A server that takes the field and drops it says nothing
//! at all: the model is never told it has tools, and every turn ends with it describing
//! the call it could not make. Nothing errors, and the session is useless.
//!
//! One request settles it. A tool the model is required to call comes back as a call from
//! a backend that honours the field, and as prose from one that does not.

use std::sync::Arc;

use futures::StreamExt;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::backend::client::StreamEvent;
use crate::backend::provider::{Provider, TurnRequest};
use crate::backend::wire::{Message, ToolSpec};

/// What the probe asks for. Named so it cannot be mistaken for a real tool in a log.
const PROBE_TOOL: &str = "lokode_probe_ping";

/// Whether the backend took the tool definitions and dropped them.
///
/// Only a definite answer counts. A request that failed proves nothing about the field -
/// it may be the model, the network or the load - and leaves the session on what it was
/// told, where the error-driven fallback is still waiting.
pub async fn ignores_native_tools(
    backend: Arc<dyn Provider>,
    model: &str,
    cancel: CancellationToken,
) -> bool {
    if !backend.capabilities().native_tools {
        return false;
    }
    let req = TurnRequest {
        model: model.to_string(),
        messages: vec![Message::user(format!(
            "Call the {PROBE_TOOL} tool. Say nothing else."
        ))],
        tools: Some(vec![ToolSpec {
            kind: "function".into(),
            function: crate::backend::wire::FunctionSpec {
                name: PROBE_TOOL.into(),
                description: "Answers a readiness check. Call it with no arguments.".into(),
                parameters: json!({"type": "object", "properties": {}}),
            },
        }]),
        grammar: None,
        max_tokens: 32,
        temperature: 0.0,
        user: None,
        // A readiness check answers in a few tokens; a reasoning model would spend them thinking.
        reason: false,
    };
    let mut stream = match backend.stream(req).await {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(why = %format!("{e:#}"), "tool probe did not answer");
            return false;
        }
    };
    let mut called = false;
    loop {
        let next = tokio::select! {
            biased;
            _ = cancel.cancelled() => return false,
            next = stream.next() => next,
        };
        match next {
            Some(Ok(StreamEvent::Call(_))) => {
                called = true;
                break;
            }
            Some(Ok(_)) => continue,
            // A stream that broke says nothing either way.
            Some(Err(e)) => {
                tracing::debug!(why = %format!("{e:#}"), "tool probe stream failed");
                return false;
            }
            None => break,
        }
    }
    if !called {
        tracing::warn!(
            model,
            "the backend took the tool definitions and ignored them"
        );
    }
    !called
}
