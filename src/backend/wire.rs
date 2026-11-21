//! OpenAI-compatible wire types: a focused subset that mirrors `crates/server/src/api/types.rs`.

use serde::{Deserialize, Serialize};

/// An image a message carries, as its bytes in base64.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Image {
    /// The media type, such as `image/png`.
    pub media_type: String,
    /// The bytes, in standard base64 with padding.
    pub data: String,
}

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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<Image>,
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
            images: Vec::new(),
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            images: Vec::new(),
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            images: Vec::new(),
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
            images: Vec::new(),
        }
    }
    pub fn tool(content: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: content.into(),
            name: None,
            tool_call_id: Some(id.into()),
            tool_calls: None,
            images: Vec::new(),
        }
    }
    pub fn with_images(mut self, images: Vec<Image>) -> Self {
        self.images = images;
        self
    }
}

/// The messages in OpenAI's dialect.
///
/// The OpenAI format accepts text only in a tool message.
/// The images of a run of consecutive tool messages follow it, in one user message.
fn openai_messages<S: serde::Serializer>(messages: &[Message], s: S) -> Result<S::Ok, S::Error> {
    let mut out: Vec<serde_json::Value> = Vec::with_capacity(messages.len());
    let mut held: Vec<&Image> = Vec::new();
    for (at, msg) in messages.iter().enumerate() {
        out.push(openai_message(msg).map_err(serde::ser::Error::custom)?);
        if msg.role == "tool" {
            held.extend(&msg.images);
        }
        let run_ends = messages.get(at + 1).is_none_or(|next| next.role != "tool");
        if run_ends && !held.is_empty() {
            let mut parts = vec![serde_json::json!({
                "type": "text",
                "text": "The images the tool results above returned.",
            })];
            parts.extend(held.drain(..).map(image_part));
            out.push(serde_json::json!({"role": "user", "content": parts}));
        }
    }
    s.collect_seq(out)
}

/// The OpenAI form of one message: itself, without its images.
///
/// A user message carries its images as parts of its content, after its text.
/// A tool message keeps its text only: its images follow in a user message.
fn openai_message(msg: &Message) -> Result<serde_json::Value, serde_json::Error> {
    let mut value = serde_json::to_value(msg)?;
    if msg.images.is_empty() {
        return Ok(value);
    }
    if let Some(fields) = value.as_object_mut() {
        fields.remove("images");
        if msg.role == "user" {
            let mut parts = vec![serde_json::json!({"type": "text", "text": msg.content})];
            parts.extend(msg.images.iter().map(image_part));
            fields.insert("content".into(), serde_json::Value::Array(parts));
        }
    }
    Ok(value)
}

/// One image as an OpenAI content part.
fn image_part(img: &Image) -> serde_json::Value {
    serde_json::json!({
        "type": "image_url",
        "image_url": {"url": format!("data:{};base64,{}", img.media_type, img.data)}
    })
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
    #[serde(serialize_with = "openai_messages")]
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
    /// What a server that failed during the answer sends in place of a chunk.
    /// An object with a `message`, or a bare string.
    #[serde(default)]
    pub error: Option<serde_json::Value>,
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
/// Times are milliseconds with a fraction.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct LokenUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefill_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decode_ms: Option<f64>,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A request with [user "look" + 1 image] gives a content array of 2 parts.
    #[test]
    fn user_with_image_serializes_as_array() {
        let images = vec![Image {
            media_type: "image/png".to_string(),
            data: "AAAA".to_string(),
        }];
        let msg = Message::user("look").with_images(images);
        let value = openai_message(&msg).unwrap();

        assert_eq!(value["role"], "user");
        let content = value["content"]
            .as_array()
            .expect("content should be an array");
        assert_eq!(content.len(), 2, "should have 2 parts");

        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "look");

        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,AAAA");
    }

    /// A request without images serializes exactly as before (content as string).
    #[test]
    fn message_without_images_serializes_as_string() {
        let msg = Message::user("hello");
        let value = openai_message(&msg).unwrap();

        assert_eq!(value["role"], "user");
        assert_eq!(value["content"], "hello");
        assert!(!value.as_object().unwrap().contains_key("images"));
    }

    /// A request without images is the JSON each message gives by itself.
    #[test]
    fn a_request_without_images_is_unchanged() {
        let call = ToolCall {
            id: "c1".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "read_file".into(),
                arguments: "{}".into(),
            },
        };
        let messages = vec![
            Message::system("rules"),
            Message::user("hi"),
            Message::assistant_calling("", vec![call]),
            Message::tool("text", "c1"),
        ];
        let request = ChatCompletionRequest {
            model: "m".into(),
            messages: messages.clone(),
            ..Default::default()
        };
        let sent = serde_json::to_value(&request).unwrap();
        let own: Vec<serde_json::Value> = messages
            .iter()
            .map(|m| serde_json::to_value(m).unwrap())
            .collect();
        assert_eq!(sent["messages"], serde_json::Value::Array(own));
        assert_eq!(sent["messages"][2]["tool_calls"][0]["type"], "function");
    }

    /// Consecutive tool results with images get a user message after the run.
    #[test]
    fn consecutive_tools_with_images_add_user_message() {
        let images1 = vec![Image {
            media_type: "image/png".to_string(),
            data: "AAAA".to_string(),
        }];
        let images2 = vec![Image {
            media_type: "image/jpeg".to_string(),
            data: "BBBB".to_string(),
        }];

        let messages = vec![
            Message::assistant("run tools").with_images(Vec::new()),
            Message::tool("result c1", "c1").with_images(images1),
            Message::tool("result c2", "c2").with_images(images2),
            Message::user("next"),
        ];

        // We need to test the full serialization
        let request = ChatCompletionRequest {
            model: "test".to_string(),
            messages,
            stream: None,
            temperature: None,
            top_p: None,
            max_tokens: None,
            seed: None,
            response_format: None,
            user: None,
            stream_options: None,
            tools: None,
            tool_choice: None,
            reasoning_effort: None,
        };

        let value = serde_json::to_value(&request).unwrap();
        let msgs: Vec<serde_json::Value> = value["messages"].as_array().unwrap().clone();

        // First: assistant (string content)
        assert_eq!(msgs[0]["role"], "assistant");
        assert_eq!(msgs[0]["content"], "run tools");

        // Second: tool c1 (string content, no `images` key)
        assert_eq!(msgs[1]["role"], "tool");
        assert_eq!(msgs[1]["content"], "result c1");
        assert!(!msgs[1].as_object().unwrap().contains_key("images"));

        // Third: tool c2 (string content, no `images` key)
        assert_eq!(msgs[2]["role"], "tool");
        assert_eq!(msgs[2]["content"], "result c2");
        assert!(!msgs[2].as_object().unwrap().contains_key("images"));

        // Fourth: user message with 3 parts (text, c1's image, c2's image)
        assert_eq!(msgs[3]["role"], "user");
        let content = msgs[3]["content"].as_array().unwrap();
        assert_eq!(content.len(), 3, "should have 3 parts");
        assert_eq!(content[0]["type"], "text");
        assert_eq!(
            content[0]["text"],
            "The images the tool results above returned."
        );
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,AAAA");
        assert_eq!(content[2]["type"], "image_url");
        assert_eq!(
            content[2]["image_url"]["url"],
            "data:image/jpeg;base64,BBBB"
        );

        // Fifth: user "next"
        assert_eq!(msgs[4]["role"], "user");
        assert_eq!(msgs[4]["content"], "next");
    }

    /// A Message with images goes through serde_json roundtrip.
    #[test]
    fn message_with_images_roundtrips() {
        let images = vec![Image {
            media_type: "image/png".to_string(),
            data: "AAAA".to_string(),
        }];
        let msg = Message::user("test").with_images(images.clone());

        let json = serde_json::to_string(&msg).unwrap();
        let msg_back: Message = serde_json::from_str(&json).unwrap();

        assert_eq!(msg_back.role, msg.role);
        assert_eq!(msg_back.images.len(), 1);
        assert_eq!(msg_back.images[0].media_type, "image/png");
        assert_eq!(msg_back.images[0].data, "AAAA");
    }

    /// A JSON message without an `images` key loads with an empty list.
    #[test]
    fn message_without_images_key_loads_empty() {
        let json = r#"{"role":"user","content":"hello","name":null,"tool_call_id":null,"tool_calls":null}"#;
        let msg: Message = serde_json::from_str(json).unwrap();
        assert!(msg.images.is_empty());
    }
}
