//! The Anthropic Messages API.
//!
//! The first provider whose wire format is genuinely different rather than a dialect, and
//! the reason the seam next door exists. Four things do not line up with the conversation
//! the agent keeps internally, and each is translated here so that nothing above has to
//! know:
//!
//! - The system prompt is a **top-level field**, not a message with a role. verve's whole
//!   compaction design rests on the system message being at index 0 and never dropped, so
//!   the invariant is kept internally and the message is lifted out at render time.
//! - A tool result is a **content block inside a `user` message**, not a message with role
//!   `tool`. Consecutive results coalesce into one user message, which is not cosmetic:
//!   splitting them across several messages teaches the model to stop making parallel
//!   calls, and parallel calls are exactly what the loop now produces.
//! - Tool calls arrive as `tool_use` blocks whose arguments stream as `input_json_delta`
//!   fragments, so the accumulator that reassembles OpenAI's fragments works here too.
//! - Streaming is a different alphabet: `content_block_start` / `content_block_delta` /
//!   `message_delta`, with the stop reason and output-token count on the last of those.
//!
//! Thinking blocks map onto the same `Reasoning` event as every other dialect, so a model
//! that reasons here looks the same on screen as one served by ollama.

use std::pin::Pin;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::stream::{Stream, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::backend::client::{ChunkStream, StreamEvent};
use crate::backend::provider::{Capabilities, Provider, TurnRequest};
use crate::backend::wire::{FunctionDelta, ToolCallDelta, Usage};

/// The API revision this speaks. Anthropic requires it on every request and treats its
/// absence as an error rather than a default.
const API_VERSION: &str = "2023-06-01";

pub struct AnthropicProvider {
    http: reqwest::Client,
    base: String,
}

impl AnthropicProvider {
    /// Build a client that carries the credential on every request.
    ///
    /// The key goes in `x-api-key`, not `Authorization` - converting a working
    /// OpenAI-compatible configuration is a header change, not a value change - and the
    /// header is marked sensitive so nothing that formats the client can echo it.
    pub fn new(base: String, api_key: &str) -> Result<Self> {
        use reqwest::header::{HeaderMap, HeaderValue};
        let mut headers = HeaderMap::new();
        let mut key = HeaderValue::from_str(api_key).context("api key is not a valid header")?;
        key.set_sensitive(true);
        headers.insert("x-api-key", key);
        headers.insert("anthropic-version", HeaderValue::from_static(API_VERSION));
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(900))
                .user_agent(concat!("verve/", env!("CARGO_PKG_VERSION")))
                .default_headers(headers)
                .build()?,
            base,
        })
    }

    fn url(&self) -> String {
        format!("{}/v1/messages", self.base.trim_end_matches('/'))
    }
}

/// Turn the internal conversation into an Anthropic request body.
///
/// Split out from the request so it can be tested without a server: the shape of this
/// translation is the whole of what makes the provider correct.
pub fn render(req: &TurnRequest, stream: bool) -> Value {
    let mut system: Option<String> = None;
    let mut messages: Vec<Value> = Vec::new();

    for m in &req.messages {
        match m.role.as_str() {
            // Lifted out of the history entirely: here it is a field, not a turn.
            "system" => {
                let existing = system.take().unwrap_or_default();
                system = Some(if existing.is_empty() {
                    m.content.clone()
                } else {
                    format!("{existing}\n\n{}", m.content)
                });
            }
            // A tool result belongs to the user turn, as a block. Consecutive results
            // join the message already being built rather than starting a new one -
            // splitting them is what stops the model making parallel calls.
            "tool" => {
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": m.tool_call_id.clone().unwrap_or_default(),
                    "content": m.content,
                });
                match messages.last_mut() {
                    Some(last)
                        if last["role"] == "user"
                            && last["content"]
                                .as_array()
                                .map(|c| c.iter().all(|b| b["type"] == "tool_result"))
                                .unwrap_or(false) =>
                    {
                        last["content"].as_array_mut().unwrap().push(block);
                    }
                    _ => messages.push(json!({"role": "user", "content": [block]})),
                }
            }
            "assistant" => {
                let mut blocks: Vec<Value> = Vec::new();
                if !m.content.is_empty() {
                    blocks.push(json!({"type": "text", "text": m.content}));
                }
                for call in m.tool_calls.iter().flatten() {
                    // Arguments travel as an object here, not the JSON string OpenAI
                    // uses. A model that produced unparseable arguments has already been
                    // repaired or refused upstream; anything still broken is sent as an
                    // empty object rather than failing the whole turn over one call.
                    let input: Value = serde_json::from_str(&call.function.arguments)
                        .unwrap_or_else(|_| json!({}));
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call.id,
                        "name": call.function.name,
                        "input": input,
                    }));
                }
                if !blocks.is_empty() {
                    messages.push(json!({"role": "assistant", "content": blocks}));
                }
            }
            _ => messages.push(json!({"role": "user", "content": m.content})),
        }
    }

    let mut body = json!({
        "model": req.model,
        // Required here, unlike everywhere else - a request without it is rejected.
        "max_tokens": req.max_tokens,
        "temperature": req.temperature,
        "messages": messages,
        "stream": stream,
    });
    if let Some(s) = system {
        body["system"] = json!(s);
    }
    if let Some(tools) = &req.tools {
        body["tools"] = Value::Array(
            tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.function.name,
                        "description": t.function.description,
                        "input_schema": t.function.parameters,
                    })
                })
                .collect(),
        );
    }
    body
}

/// What one streaming event carries, before it becomes a `StreamEvent`.
#[derive(Debug, Deserialize)]
struct Event {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    index: usize,
    #[serde(default)]
    content_block: Option<Block>,
    #[serde(default)]
    delta: Option<Delta>,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
    #[serde(default)]
    message: Option<MessageStart>,
}

#[derive(Debug, Deserialize)]
struct MessageStart {
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
struct Block {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Delta {
    // Deliberately not read: which field is present already says which kind of delta this
    // is, and branching on presence survives a new delta type being added beside the ones
    // handled here rather than falling through a name match.
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    partial_json: Option<String>,
    /// Present on `message_delta`, which is where generation's ending is reported.
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
}

/// Translate one Anthropic event into the events the loop reads.
fn events_of(e: Event) -> Vec<StreamEvent> {
    match e.kind.as_str() {
        // A tool call opens with its id and name and then streams its arguments, so the
        // opening event is what tells the accumulator which index is which call.
        "content_block_start" => match e.content_block {
            Some(b) if b.kind == "tool_use" => vec![StreamEvent::Call(ToolCallDelta {
                index: e.index,
                id: b.id,
                function: Some(FunctionDelta {
                    name: b.name,
                    arguments: None,
                }),
            })],
            _ => Vec::new(),
        },
        "content_block_delta" => match e.delta {
            Some(d) => {
                if let Some(t) = d.text {
                    vec![StreamEvent::Delta(t)]
                } else if let Some(t) = d.thinking {
                    // The same event every other dialect's scratchpad becomes.
                    vec![StreamEvent::Reasoning(t)]
                } else if let Some(fragment) = d.partial_json {
                    vec![StreamEvent::Call(ToolCallDelta {
                        index: e.index,
                        id: None,
                        function: Some(FunctionDelta {
                            name: None,
                            arguments: Some(fragment),
                        }),
                    })]
                } else {
                    Vec::new()
                }
            }
            None => Vec::new(),
        },
        // Where the stop reason and the output count arrive. `tool_use` means the model
        // wants tools run, which the loop already reads off the calls themselves; the one
        // that has to survive translation is the truncation case.
        "message_delta" => {
            let finish_reason = e
                .delta
                .and_then(|d| d.stop_reason)
                .map(|r| match r.as_str() {
                    "max_tokens" => "length".to_string(),
                    "tool_use" => "tool_calls".to_string(),
                    other => other.to_string(),
                });
            vec![StreamEvent::End {
                finish_reason,
                usage: e.usage.map(|u| Usage {
                    prompt_tokens: u.input_tokens,
                    completion_tokens: u.output_tokens,
                }),
            }]
        }
        // The prompt token count arrives up front rather than at the end.
        "message_start" => match e.message.and_then(|m| m.usage) {
            Some(u) if u.input_tokens > 0 => vec![StreamEvent::End {
                finish_reason: None,
                usage: Some(Usage {
                    prompt_tokens: u.input_tokens,
                    completion_tokens: 0,
                }),
            }],
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn label(&self) -> &str {
        "anthropic"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            native_tools: true,
            // Anthropic does have a schema-constrained output mode, but it is a different
            // parameter with different restrictions from the one the envelope grammar is
            // built for. Reporting false keeps the agent on native tool calling here
            // rather than handing over a schema that would be accepted and not applied.
            grammar: false,
            usage_in_stream: true,
        }
    }

    async fn models(&self) -> Result<Vec<String>> {
        let url = format!("{}/v1/models", self.base.trim_end_matches('/'));
        let resp = self.http.get(&url).send().await.context("GET /v1/models")?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("{url} answered {status}");
        }
        let body: serde_json::Value = serde_json::from_str(&body).context("decoding /v1/models")?;
        Ok(body["data"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter_map(|m| m["id"].as_str())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn stream(&self, req: TurnRequest) -> Result<ChunkStream> {
        let resp = self
            .http
            .post(self.url())
            .json(&render(&req, true))
            .send()
            .await
            .context("POST /v1/messages")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("anthropic {status}: {body}"));
        }

        let mapped = resp.bytes_stream().eventsource().flat_map(|ev| {
            let events: Vec<Result<StreamEvent>> = match ev {
                Err(e) => vec![Err(anyhow!("sse: {e}"))],
                Ok(event) => match serde_json::from_str::<Event>(&event.data) {
                    Ok(parsed) => events_of(parsed).into_iter().map(Ok).collect(),
                    Err(e) => vec![Err(anyhow!("decode event: {e}: raw={}", event.data))],
                },
            };
            futures::stream::iter(events)
        });
        Ok(Box::pin(mapped) as Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>)
    }

    async fn complete(&self, req: TurnRequest) -> Result<String> {
        let resp = self
            .http
            .post(self.url())
            .json(&render(&req, false))
            .send()
            .await
            .context("POST /v1/messages")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("anthropic {status}: {body}"));
        }
        let body: Value = resp.json().await.context("decoding response")?;
        Ok(body["content"]
            .as_array()
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|b| b["type"] == "text")
                    .filter_map(|b| b["text"].as_str())
                    .collect::<String>()
            })
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::wire::{FunctionCall, Message, ToolCall};

    fn req(messages: Vec<Message>) -> TurnRequest {
        TurnRequest {
            model: "claude-x".into(),
            messages,
            tools: None,
            grammar: None,
            max_tokens: 1024,
            temperature: 0.2,
            user: None,
        }
    }

    /// The invariant verve's compaction rests on is that the system message is history[0].
    /// Anthropic has no such role, so it is lifted into a field rather than sent as a turn.
    #[test]
    fn the_system_message_becomes_a_field() {
        let body = render(
            &req(vec![
                Message::system("you are a code agent"),
                Message::user("hello"),
            ]),
            true,
        );
        assert_eq!(body["system"], "you are a code agent");
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1, "the system turn was sent as a message");
        assert_eq!(msgs[0]["role"], "user");
    }

    /// Not cosmetic: results split across several user messages teach the model to stop
    /// making parallel calls, and parallel calls are what the loop now produces.
    #[test]
    fn consecutive_tool_results_coalesce_into_one_user_message() {
        let body = render(
            &req(vec![
                Message::user("read them"),
                Message::tool("AAA", "c1"),
                Message::tool("BBB", "c2"),
            ]),
            true,
        );
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2, "{msgs:#?}");
        let blocks = msgs[1]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2, "results were split across messages");
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[0]["tool_use_id"], "c1");
        assert_eq!(blocks[1]["tool_use_id"], "c2");
    }

    /// A result that follows a fresh assistant turn starts its own message rather than
    /// joining an unrelated earlier one.
    #[test]
    fn results_do_not_join_across_an_assistant_turn() {
        let body = render(
            &req(vec![
                Message::tool("first", "c1"),
                Message::assistant("thinking about it"),
                Message::tool("second", "c2"),
            ]),
            true,
        );
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3, "{msgs:#?}");
        assert_eq!(msgs[2]["content"][0]["tool_use_id"], "c2");
    }

    /// Arguments are an object here, not the JSON string OpenAI carries them in.
    #[test]
    fn tool_calls_are_rendered_as_blocks_with_parsed_input() {
        let mut m = Message::assistant("");
        m.tool_calls = Some(vec![ToolCall {
            id: "c1".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "read_file".into(),
                arguments: r#"{"path":"a.rs"}"#.into(),
            },
        }]);
        let body = render(&req(vec![m]), true);
        let block = &body["messages"][0]["content"][0];
        assert_eq!(block["type"], "tool_use");
        assert_eq!(block["name"], "read_file");
        assert_eq!(
            block["input"]["path"], "a.rs",
            "arguments were left as a string"
        );
    }

    /// One malformed call must not take the whole turn down with it.
    #[test]
    fn unparseable_arguments_become_an_empty_object() {
        let mut m = Message::assistant("");
        m.tool_calls = Some(vec![ToolCall {
            id: "c1".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "read_file".into(),
                arguments: "{not json".into(),
            },
        }]);
        let body = render(&req(vec![m]), true);
        assert!(body["messages"][0]["content"][0]["input"].is_object());
    }

    #[test]
    fn max_tokens_is_always_present() {
        let body = render(&req(vec![Message::user("hi")]), true);
        assert_eq!(
            body["max_tokens"], 1024,
            "the API rejects a request without it"
        );
    }

    fn ev(json_text: &str) -> Vec<StreamEvent> {
        events_of(serde_json::from_str(json_text).unwrap())
    }

    #[test]
    fn text_deltas_become_prose() {
        let out = ev(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
        );
        assert!(
            matches!(&out[..], [StreamEvent::Delta(t)] if t == "hi"),
            "{out:?}"
        );
    }

    /// A thinking block is the same event every other dialect's scratchpad becomes.
    #[test]
    fn thinking_deltas_become_reasoning() {
        let out = ev(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}"#,
        );
        assert!(
            matches!(&out[..], [StreamEvent::Reasoning(t)] if t == "hmm"),
            "{out:?}"
        );
    }

    /// The opening event carries the id and name; the arguments arrive after it.
    #[test]
    fn a_tool_call_is_assembled_from_its_start_and_its_fragments() {
        let start = ev(
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"grep"}}"#,
        );
        match &start[..] {
            [StreamEvent::Call(d)] => {
                assert_eq!(d.index, 1);
                assert_eq!(d.id.as_deref(), Some("toolu_1"));
                assert_eq!(d.function.as_ref().unwrap().name.as_deref(), Some("grep"));
            }
            other => panic!("{other:?}"),
        }
        let frag = ev(
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"q\":"}}"#,
        );
        match &frag[..] {
            [StreamEvent::Call(d)] => {
                assert_eq!(d.index, 1, "the fragment must land on the same call");
                assert_eq!(
                    d.function.as_ref().unwrap().arguments.as_deref(),
                    Some("{\"q\":")
                );
            }
            other => panic!("{other:?}"),
        }
    }

    /// Truncation is the stop reason that has to survive translation: a cut-off answer is
    /// otherwise indistinguishable from a model that cannot produce the shape asked for.
    #[test]
    fn stop_reasons_are_translated_to_the_ones_the_loop_reads() {
        let out = ev(
            r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":7}}"#,
        );
        match &out[..] {
            [StreamEvent::End { finish_reason, .. }] => {
                assert_eq!(finish_reason.as_deref(), Some("length"));
            }
            other => panic!("{other:?}"),
        }
        let out = ev(r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#);
        match &out[..] {
            [StreamEvent::End { finish_reason, .. }] => {
                assert_eq!(finish_reason.as_deref(), Some("tool_calls"));
            }
            other => panic!("{other:?}"),
        }
    }

    /// The prompt count arrives up front here, and it is what calibrates the context
    /// budget - losing it would leave the chars-per-token estimate uncorrected.
    #[test]
    fn the_prompt_token_count_is_reported_from_the_opening_event() {
        let out = ev(
            r#"{"type":"message_start","message":{"usage":{"input_tokens":312,"output_tokens":0}}}"#,
        );
        match &out[..] {
            [StreamEvent::End { usage, .. }] => {
                assert_eq!(usage.unwrap().prompt_tokens, 312);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unknown_events_are_ignored_rather_than_failing_the_turn() {
        assert!(ev(r#"{"type":"ping"}"#).is_empty());
        assert!(ev(r#"{"type":"content_block_stop","index":0}"#).is_empty());
    }
}
