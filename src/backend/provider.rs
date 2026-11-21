//! What the agent needs from a model, stated without reference to whose API answers it.
//!
//! The loop reasons about a turn: a conversation goes out, prose and tool calls come back.
//!
//! Everything below that belongs behind this trait.
//! That covers the endpoint and the header that carries the credential.
//! It covers whether the system prompt is a message or a field.
//! It covers whether a tool result is its own role or a block inside a user message.
//!
//! The internal conversation stays OpenAI-shaped, which is what almost every server wants.
//! A provider whose wire format differs does the translation itself.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::backend::client::ChunkStream;
use crate::backend::wire::{Message, ToolSpec};

/// One request for one model response.
///
/// Neutral by construction: no field here names a vendor.
/// A provider that cannot honour a field says so through [`Capabilities`].
pub struct TurnRequest {
    pub model: String,
    pub messages: Vec<Message>,
    /// The tools, when asking for them natively.
    /// `None` under the envelope protocol, where the manifest travels in the system prompt.
    pub tools: Option<Vec<ToolSpec>>,
    /// A JSON Schema the answer must match, when the server can enforce one.
    ///
    /// This is how the envelope protocol is made reliable.
    /// A provider that reports no grammar support is never handed one.
    pub grammar: Option<Value>,
    pub max_tokens: u32,
    /// `None` leaves the sampling to the server, which applies what the model recommends.
    pub temperature: Option<f32>,
    /// The session id, passed where a backend can use it to key its own caches.
    pub user: Option<String>,
    /// Whether the model may reason before it answers.
    /// A short question beside the turn, such as a guess or a review, asks for no reasoning.
    /// Its whole budget then goes to the answer.
    pub reason: bool,
    /// What the request is for, which is what its measured cost is counted against.
    pub purpose: crate::backend::meter::Purpose,
}

/// What a provider can actually do, so the agent asks for nothing it cannot deliver.
///
/// Capabilities are declared, not discovered.
/// A rejected request is the only signal a server gives, and its wording differs per server.
/// The fallback on a rejected request exists for one case: a backend that refuses native tool
/// calls.
#[derive(Debug, Clone, Copy)]
pub struct Capabilities {
    /// Whether tools can travel in their own field.
    /// Calls are answered against ids the server issued.
    pub native_tools: bool,
    /// Whether the server will hold the answer to a JSON Schema.
    ///
    /// False means the envelope protocol is unconstrained there.
    /// The model is asked for a shape and trusted to produce it.
    /// The repair pass handles a wrong shape.
    pub grammar: bool,
    /// Whether a token count comes back with the stream.
    /// Without it the chars-per-token estimate is never corrected.
    /// The context budget then stays an estimate.
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
    /// Used only by the repair round-trip, which wants a small JSON object as fast as possible.
    /// It has nothing to show while it waits, so it does not stream.
    async fn complete(&self, req: TurnRequest) -> Result<String>;

    /// Have the backend hold `model` ready, without asking it anything.
    ///
    /// A local server loads the model onto its device to answer the first request.
    /// A warm model answers at once.
    ///
    /// The default does nothing, which is right for a hosted API.
    /// There is nothing to warm there, and the attempt would be billed.
    async fn warm(&self, _model: &str) -> Result<()> {
        Ok(())
    }

    /// The context window the server says it served, once it has said.
    ///
    /// `None` from a provider whose API has no way to say it.
    /// The caller then keeps the window it was configured with.
    fn served_window(&self) -> Option<usize> {
        None
    }

    /// What the backend says it serves, for choosing one without having to know its name.
    ///
    /// A backend that does not answer is not a broken backend.
    /// The caller falls back to what it was told.
    /// This returns the error rather than an empty list, which would read as a server that
    /// serves nothing.
    async fn models(&self) -> Result<Vec<String>>;

    /// Whether `model` takes images, when the backend says.
    ///
    /// `None` when it has no way to say.
    async fn sees_images(&self, _model: &str) -> Option<bool> {
        None
    }
}
