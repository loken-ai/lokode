//! OpenAI-compatible wire types — a focused subset that mirrors
//! `crates/server/src/api/types.rs`. Copied (rather than path-depended)
//! so the TUI evolves independently from the server crate.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Calls an assistant turn made, in the native protocol.
    ///
    /// Sent back verbatim on the next request: a `role: "tool"` message is only meaningful
    /// against the call it answers, and a server that checks will reject one whose
    /// `tool_call_id` matches nothing it was shown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

/// One call, as the wire carries it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "function_kind")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    /// The arguments object, as a string of JSON.
    ///
    /// A string, not an object - that is what the format says, and it is why a model can
    /// get the JSON wrong here in exactly the ways the repair pass already knows about.
    pub arguments: String,
}

fn function_kind() -> String {
    "function".into()
}

/// A tool offered to the model in the native protocol.
#[derive(Debug, Clone, Serialize)]
pub struct ToolSpec {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionSpec,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: content.into(),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }
    /// An assistant turn that asked for tools to be run.
    pub fn assistant_calling(content: impl Into<String>, calls: Vec<ToolCall>) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
            name: None,
            tool_call_id: None,
            tool_calls: Some(calls),
        }
    }
    pub fn tool(content: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: content.into(),
            name: None,
            tool_call_id: Some(id.into()),
            tool_calls: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ResponseFormat {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub json_schema: Option<serde_json::Value>,
}

impl ResponseFormat {
    pub fn json_schema(name: &str, schema: serde_json::Value) -> Self {
        Self {
            kind: "json_schema".into(),
            json_schema: Some(serde_json::json!({
                "name": name,
                "strict": true,
                "schema": schema,
            })),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,
    /// Server reuses prompt KV cache when this stays constant across turns.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Asks a streaming response to end with a usage report.
    ///
    /// Dropped and the request retried without it if the server rejects the field, since
    /// not every OpenAI-compatible implementation accepts it and the token counts are
    /// worth having where they are on offer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    /// Tools offered in the native protocol. Absent in the envelope protocol, where the
    /// tools are described in the system prompt and constrained by `response_format`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

// ---------------------------------------------------------------- Responses --

#[derive(Debug, Clone, Deserialize)]
pub struct ChatChunk {
    #[serde(default)]
    pub choices: Vec<ChatChunkChoice>,
    /// Present on the final chunk when the server was asked for it.
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatChunkChoice {
    #[serde(default)]
    pub delta: ChunkDelta,
    /// Why generation stopped. `"length"` means the answer was cut off, which is
    /// otherwise indistinguishable from a model that emits malformed JSON.
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// What the server counted, which is the only tokenizer this client has access to.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChunkDelta {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCallDelta>>,
    /// A reasoning model's scratchpad, where the server splits it out for us.
    ///
    /// Not in the OpenAI specification, so the vendors disagree on the name: ollama and
    /// OpenRouter say `reasoning`, DeepSeek says `reasoning_content`, and loken leaves it
    /// inline in `content` behind `<think>` tags. Both names are read here; the inline
    /// form is handled by the splitter.
    #[serde(default)]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub reasoning_content: Option<String>,
}

/// A fragment of a tool call.
///
/// The name arrives once and the arguments arrive a few characters at a time, so the
/// pieces have to be reassembled by `index` before any of it means anything.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolCallDelta {
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<FunctionDelta>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FunctionDelta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}
