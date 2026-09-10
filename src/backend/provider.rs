//! What the agent needs from a model, stated without reference to whose API answers it.
//!
//! The loop reasons about a turn: a conversation goes out, prose and tool calls come back.
//! Everything below that - which endpoint, which header carries the credential, whether the
//! system prompt is a message or a field, whether a tool result is its own role or a block
//! inside a user message - is a property of one vendor and belongs behind this trait.
//!
//! The internal conversation stays OpenAI-shaped, because that is what almost every server
//! wants and shaping it otherwise would tax the common case to serve the uncommon one. A
//! provider whose wire format differs pays the translation itself.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::backend::client::ChunkStream;
use crate::backend::wire::{Message, ToolSpec};

/// One request for one model response.
///
/// Neutral by construction: no field here names a vendor, and a provider that cannot honour
/// one says so through [`Capabilities`] rather than by failing at request time.
pub struct TurnRequest {
    pub model: String,
    pub messages: Vec<Message>,
    /// The tools, when asking for them natively. `None` under the envelope protocol, where
    /// the manifest travels in the system prompt instead.
    pub tools: Option<Vec<ToolSpec>>,
    /// A JSON Schema the answer must match, when the server can enforce one.
    ///
    /// This is how the envelope protocol is made reliable; a provider that reports no
    /// grammar support will never be handed one.
    pub grammar: Option<Value>,
    pub max_tokens: u32,
    pub temperature: f32,
    /// The session id, passed where a backend can use it to key its own caches.
    pub user: Option<String>,
}

/// What a provider can actually do, so the agent asks for nothing it cannot deliver.
///
/// Declared rather than discovered. Discovery is attractive and fragile: the only signal a
/// server gives is a rejected request, matched on prose that every implementation words
/// differently. That fallback still exists as a safety net for the one case worth catching -
/// a backend that refuses native tool calls - but it is a net, not the design.
#[derive(Debug, Clone, Copy)]
pub struct Capabilities {
    /// Whether tools can travel in their own field, with calls answered against ids the
    /// server issued.
    pub native_tools: bool,
    /// Whether the server will hold the answer to a JSON Schema.
    ///
    /// False means the envelope protocol is unconstrained there - the model is asked for a
    /// shape and merely trusted to produce it, which is what the repair pass exists for.
    pub grammar: bool,
    /// Whether a token count comes back with the stream. Without it the chars-per-token
    /// estimate never gets corrected, and the context budget stays a guess.
    pub usage_in_stream: bool,
}

#[async_trait]
pub trait Provider: Send + Sync {
    /// A name for the log and the status line.
    fn label(&self) -> &str;

    fn capabilities(&self) -> Capabilities;

    /// Ask for one response, streamed.
    async fn stream(&self, req: TurnRequest) -> Result<ChunkStream>;

    /// Ask for one response and wait for all of it.
    ///
    /// Used only by the repair round-trip, which wants a small JSON object as fast as
    /// possible and has nothing to show while it waits - streaming it would add a frame
    /// budget to a request whose whole point is to be over quickly.
    async fn complete(&self, req: TurnRequest) -> Result<String>;

    /// Have the backend hold `model` ready, without asking it anything worth reading.
    ///
    /// A local server loads gigabytes onto a card to answer the first request; a warm
    /// model answers at once. The default does nothing, which is right for a hosted API:
    /// there is nothing there to warm and the attempt would be billed.
    async fn warm(&self, _model: &str) -> Result<()> {
        Ok(())
    }

    /// What the backend says it serves, for choosing one without having to know its name.
    ///
    /// A backend that does not answer is not a broken backend: the caller falls back to
    /// what it was told, so this returns the error rather than an empty list, which would
    /// read as a server that serves nothing.
    async fn models(&self) -> Result<Vec<String>>;
}
