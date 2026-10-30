//! OpenAI-compatible wire types: a focused subset that mirrors `crates/server/src/api/types.rs`.

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
    /// Sent back verbatim on the next request.
    /// A `role: "tool"` message is only meaningful against the call it answers.
    /// A server that checks rejects one whose `tool_call_id` matches nothing it was shown.
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
    /// The format makes it a string, not an object.
    /// A model can get the JSON wrong here, in the ways the repair pass handles.
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

#[derive(Debug, Clone, Default, Serialize)]
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
    /// The server reuses its prompt KV cache when this stays constant across turns.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Asks a streaming response to end with a usage report.
    ///
    /// Dropped, and the request retried without it, if the server rejects the field.
    /// Not every OpenAI-compatible implementation accepts it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    /// Tools offered in the native protocol.
    /// Absent in the envelope protocol, where the tools are described in the system prompt
    /// and constrained by `response_format`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<String>,
    /// How much a reasoning model thinks before answering.
    /// Sent only to ask for as little as it allows.
    /// Dropped for the session by a server that rejects the field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
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
    /// Why generation stopped.
    /// `"length"` means the answer was cut off, which looks like a model that emits malformed
    /// JSON.
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
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptDetails>,
    #[serde(default)]
    pub completion_tokens_details: Option<CompletionDetails>,
    /// What a loken server measured of the request.
    /// Absent from any other server.
    #[serde(default)]
    pub loken: Option<LokenUsage>,
}

/// The part of the prompt the server already held.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptDetails {
    #[serde(default)]
    pub cached_tokens: u32,
}

/// What speculative decoding kept and threw away.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionDetails {
    #[serde(default)]
    pub accepted_prediction_tokens: u32,
    #[serde(default)]
    pub rejected_prediction_tokens: u32,
}

/// What a loken server measured of one request.
/// A field it could not measure is absent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct LokenUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefill_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decode_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub energy: Option<Energy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kv: Option<Kv>,
}

/// The energy one request drew, in joules.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Energy {
    pub joules: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_joules: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_joules: Option<f64>,
    /// The processor's part was estimated, its counters being unreadable.
    #[serde(default)]
    pub estimated: bool,
    #[serde(default)]
    pub share: Share,
}

/// How a request's energy was told apart from that of requests running beside it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Share {
    /// It ran alone, so the energy is its own.
    #[default]
    Alone,
    /// Others ran at the same time, and each sample was split by the tokens each request
    /// computed over it, or evenly where none computed any.
    Work,
    /// A way of sharing this client does not know.
    #[serde(other)]
    Unknown,
}

/// The request's sequence in the key-value cache, against the cache's capacity.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Kv {
    pub used_tokens: u64,
    pub capacity_tokens: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChunkDelta {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCallDelta>>,
    /// A reasoning model's scratchpad, where the server splits it out.
    ///
    /// Not in the OpenAI specification, so the vendors differ on the name.
    /// ollama and OpenRouter say `reasoning`.
    /// DeepSeek says `reasoning_content`.
    /// loken leaves it inline in `content` behind `<think>` tags, which the splitter handles.
    /// Both field names are read here.
    #[serde(default)]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub reasoning_content: Option<String>,
}

/// A fragment of a tool call.
///
/// The name arrives once and the arguments arrive a few characters at a time.
/// The pieces are reassembled by `index` before any of it means anything.
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
