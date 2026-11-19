use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use eventsource_stream::Eventsource;
use futures::stream::{Stream, StreamExt};

use crate::backend::meter::{ledger, Purpose};
use crate::backend::wire::{
    ChatChunk, ChatCompletionRequest, ResponseFormat, StreamOptions, ToolCallDelta, Usage,
};

/// What a streaming response hands back as it arrives.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    Delta(String),
    /// A piece of the model's scratchpad, however the server chose to mark it.
    ///
    /// Shown and then dropped.
    /// It never enters the conversation, because it is not what the model said.
    Reasoning(String),
    /// A fragment of a native tool call, to be reassembled by index.
    Call(ToolCallDelta),
    /// The server said it is still at work and has nothing to send yet: a keep-alive.
    Alive,
    /// The server is reading the prompt, and says how far it has got, in tokens.
    Prefill {
        done: u64,
        total: u64,
    },
    /// The end of generation, carrying what the server was willing to say about it.
    End {
        finish_reason: Option<String>,
        usage: Option<Usage>,
    },
}

/// Raised when a server will not take something the request asked for.
///
/// Distinct from an ordinary refusal.
/// The caller can drop the feature and try again rather than give up on the turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unsupported {
    Usage,
    NativeTools,
}

pub type ChunkStream = Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>;

/// How many times to try again after a failure that looks temporary.
const MAX_ATTEMPTS: usize = 3;

/// How long a connection may stay silent before the client gives up.
pub(crate) const READ_TIMEOUT: Duration = Duration::from_secs(900);

/// Wait before the first retry.
/// Doubled after each retry.
const BACKOFF_BASE: Duration = Duration::from_millis(400);

pub struct BackendClient {
    http: reqwest::Client,
    base: String,
    /// Cleared the first time the server rejects `stream_options`.
    /// Later requests omit the field.
    usage_supported: AtomicBool,
    /// Whether the server takes `reasoning_effort`.
    /// Learned the way `usage_supported` is.
    effort_supported: AtomicBool,
    /// The widest window the server takes for the model last asked about.
    /// Set from the model list of the server once learned.
    served_window: std::sync::Mutex<Option<(String, usize)>>,
}

impl BackendClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self::with_headers(base_url, &[])
    }

    /// A client that sends `headers` with every request.
    ///
    /// Headers are built into the client's defaults.
    /// That gives one place to attach a credential, and no request path that can omit it.
    /// Headers that will not encode are dropped with a warning naming the header.
    /// The warning never carries the value, which must not reach a log.
    pub fn with_headers(base_url: impl Into<String>, headers: &[(String, String)]) -> Self {
        let mut map = reqwest::header::HeaderMap::new();
        for (name, value) in headers {
            match (
                name.parse::<reqwest::header::HeaderName>(),
                reqwest::header::HeaderValue::from_str(value),
            ) {
                (Ok(n), Ok(mut v)) => {
                    // Marked sensitive so it is not echoed by anything that formats the
                    // client's state.
                    v.set_sensitive(true);
                    map.insert(n, v);
                }
                _ => tracing::warn!(header = %name, "unusable header, not sent"),
            }
        }

        Self {
            http: reqwest::Client::builder()
                // The timeout bounds silence, not total duration.
                // A stream the server keeps alive runs as long as it produces.
                // An answer that is not streamed is still bounded.
                .read_timeout(READ_TIMEOUT)
                .default_headers(map)
                .build()
                .expect("reqwest client"),
            base: base_url.into(),
            usage_supported: AtomicBool::new(true),
            effort_supported: AtomicBool::new(true),
            served_window: std::sync::Mutex::new(None),
        }
    }

    /// Learn the widest window the server takes for `model` from its model list, once per model.
    /// A server serves each request with the window it needs up to that one.
    /// It names that window `max_model_len`.
    /// A server that does not say leaves the window unknown.
    async fn learn_window(&self, model: &str) {
        let known = self
            .served_window
            .lock()
            .map(|w| w.as_ref().is_some_and(|(m, _)| m == model))
            .unwrap_or(true);
        if known {
            return;
        }
        let url = crate::backend::presets::endpoint(&self.base, "models");
        let Ok(resp) = self.http.get(&url).send().await else {
            return;
        };
        let Ok(body) = resp.json::<serde_json::Value>().await else {
            return;
        };
        if let Some(n) = model_window(&body, model) {
            if let Ok(mut w) = self.served_window.lock() {
                *w = Some((model.to_string(), n));
            }
        }
    }

    /// The widest window the server takes for the model last asked about, if it has said.
    pub fn served_window(&self) -> Option<usize> {
        self.served_window
            .lock()
            .ok()
            .and_then(|w| w.as_ref().map(|(_, n)| *n))
    }

    fn url(&self) -> String {
        crate::backend::presets::endpoint(&self.base, "chat/completions")
    }

    /// Non-streaming chat completion that returns the assistant content as a single string.
    /// Convenience for repair round-trips.
    async fn complete_request(
        &self,
        mut req: ChatCompletionRequest,
        purpose: Purpose,
    ) -> Result<String> {
        req.stream = Some(false);
        req.stream_options = None;
        let url = self.url();

        let text = loop {
            if !self.effort_supported.load(Ordering::Relaxed) {
                req.reasoning_effort = None;
            }
            let asked_for_effort = req.reasoning_effort.is_some();
            let attempt = self
                .with_retries(|| {
                    let req = req.clone();
                    let url = url.clone();
                    async move {
                        let resp = self
                            .http
                            .post(&url)
                            .json(&req)
                            .send()
                            .await
                            .map_err(|e| classify_transport(e, &url))?;
                        let status = resp.status();
                        let body = resp.text().await.map_err(|e| Retry::Fatal(e.into()))?;
                        if !status.is_success() {
                            return Err(classify(status, &body));
                        }
                        Ok(body)
                    }
                })
                .await;
            match attempt {
                Ok(body) => break body,
                // A server that does not know the field says so with a 4xx.
                // Ask without it.
                Err(e) if asked_for_effort && mentions(&e, &["reasoning_effort"]) => {
                    tracing::warn!(why = %e, "backend will not take reasoning_effort; asking without it");
                    self.effort_supported.store(false, Ordering::Relaxed);
                }
                Err(e) => return Err(e),
            }
        };

        let body: serde_json::Value =
            serde_json::from_str(&text).with_context(|| "decoding chat completion response")?;
        if let Some(error) = body.get("error").filter(|e| !e.is_null()) {
            return Err(stream_failure(error));
        }
        if let Ok(usage) = serde_json::from_value::<Usage>(body["usage"].clone()) {
            ledger().record(purpose, &usage);
        }
        Ok(body["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_string())
    }

    /// Streaming chat completion.
    ///
    /// Retries cover establishing the response only.
    /// Once bytes are flowing the answer is partly on screen and partly in the transcript.
    /// Starting again would duplicate what the user has already read.
    async fn stream_request(
        &self,
        mut req: ChatCompletionRequest,
        purpose: Purpose,
    ) -> Result<ChunkStream> {
        req.stream = Some(true);
        let url = self.url();

        let resp = loop {
            req.stream_options =
                self.usage_supported
                    .load(Ordering::Relaxed)
                    .then_some(StreamOptions {
                        include_usage: true,
                    });
            let asked_for_usage = req.stream_options.is_some();

            let attempt = self
                .with_retries(|| {
                    let req = req.clone();
                    let url = url.clone();
                    async move {
                        let resp = self
                            .http
                            .post(&url)
                            .json(&req)
                            .send()
                            .await
                            .map_err(|e| classify_transport(e, &url))?;
                        let status = resp.status();
                        if !status.is_success() {
                            let body = resp.text().await.unwrap_or_default();
                            return Err(classify(status, &body));
                        }
                        Ok(resp)
                    }
                })
                .await;

            match attempt {
                Ok(resp) => break resp,
                // A server that does not know the field says so with a 4xx.
                // Give up the token counts rather than the request.
                Err(e) if asked_for_usage && mentions(&e, &["stream_options", "include_usage"]) => {
                    tracing::warn!(
                        why = %e,
                        "backend will not take stream_options; giving up token counts for this session"
                    );
                    self.usage_supported.store(false, Ordering::Relaxed);
                    continue;
                }
                // It is not worked around here.
                // The caller has to change protocol.
                // That means changing the system prompt and how the answer is read.
                Err(e) if req.tools.is_some() && mentions(&e, &["tools", "tool_choice"]) => {
                    tracing::warn!(why = %e, "backend will not take native tool calls");
                    return Err(e.context(UNSUPPORTED_NATIVE_TOOLS));
                }
                Err(e) => return Err(e),
            }
        };

        self.learn_window(&req.model).await;

        // One splitter per response.
        // The tags it tracks can straddle chunk boundaries.
        // Its state outlives a single chunk and does not outlive the stream.
        //
        // A keep-alive is an SSE comment, which an event parser drops.
        // It is turned into an event of its own.
        // That tells the caller that a server reading a long prompt is working.
        let sse = resp
            .bytes_stream()
            .map(|chunk| {
                chunk.map(|b| match comment_event(&b) {
                    Some(event) => event.into(),
                    None => b,
                })
            })
            .eventsource();
        // One chunk can carry several things at once.
        // It can carry a piece of prose, fragments of more than one tool call, and the reason
        // generation ended.
        // A chunk becomes a list of events rather than one.
        //
        // One splitter per response, moved into the closure.
        // The tags it tracks can straddle chunk boundaries.
        // Its state outlives a single chunk and does not outlive the stream.
        // A half-open tag of one response never reaches the next.
        let mut think = crate::backend::reasoning::ThinkSplitter::default();
        let mapped = sse.flat_map(move |ev| {
            let events: Vec<Result<StreamEvent>> = match ev {
                Err(e) => vec![Err(anyhow!("sse: {e}"))],
                Ok(event) if event.event == "alive" => vec![Ok(StreamEvent::Alive)],
                Ok(event) if event.event == "prefill" => vec![Ok(prefill_of(&event.data))],
                Ok(event) if event.data.trim() == "[DONE]" => {
                    let _ = event;
                    Vec::new()
                }
                Ok(event) => match serde_json::from_str::<ChatChunk>(&event.data) {
                    Ok(ChatChunk {
                        error: Some(error), ..
                    }) => vec![Err(stream_failure(&error))],
                    Ok(c) => {
                        let events = events_of(c, &mut think);
                        for e in &events {
                            if let StreamEvent::End { usage: Some(u), .. } = e {
                                ledger().record(purpose, u);
                            }
                        }
                        events.into_iter().map(Ok).collect()
                    }
                    Err(e) => vec![Err(anyhow!("decode chunk: {e}: raw={}", event.data))],
                },
            };
            futures::stream::iter(events)
        });

        Ok(Box::pin(mapped))
    }

    /// Try again after failures that a second attempt could plausibly fix.
    async fn with_retries<T, F, Fut>(&self, mut make: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, Retry>>,
    {
        let mut delay = BACKOFF_BASE;
        for attempt in 1..=MAX_ATTEMPTS {
            match make().await {
                Ok(v) => return Ok(v),
                Err(Retry::Fatal(e)) => return Err(e),
                Err(Retry::Transient(e)) => {
                    if attempt == MAX_ATTEMPTS {
                        tracing::error!(why = %e, "backend unreachable after {MAX_ATTEMPTS} attempts");
                        return Err(e.context(format!("gave up after {MAX_ATTEMPTS} attempts")));
                    }
                    tracing::warn!(
                        attempt,
                        retry_in_ms = delay.as_millis(),
                        why = %e,
                        "backend failed in a way worth retrying"
                    );
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
            }
        }
        unreachable!("the loop returns on its last attempt")
    }
}

/// Whether `chunk` holds nothing but SSE comment lines.
fn is_keep_alive(chunk: &[u8]) -> bool {
    let text = String::from_utf8_lossy(chunk);
    let mut lines = text.lines().filter(|l| !l.is_empty()).peekable();
    lines.peek().is_some() && lines.all(|l| l.starts_with(':'))
}

/// The marker a loken server puts on the comment that says how far it has read a prompt.
const PREFILL_COMMENT: &str = "loken-prefill";

/// A chunk of comments alone, as the event it stands for, which an event parser would
/// otherwise drop.
/// That is the last progress of a prompt being read when there is one, a keep-alive when not.
/// `None` for a chunk that carries anything else.
pub(crate) fn comment_event(chunk: &[u8]) -> Option<Vec<u8>> {
    if !is_keep_alive(chunk) {
        return None;
    }
    let text = String::from_utf8_lossy(chunk);
    let progress = text.lines().rev().find_map(|l| {
        l.strip_prefix(':')?
            .trim_start()
            .strip_prefix(PREFILL_COMMENT)
            .map(str::trim)
    });
    Some(match progress {
        Some(json) => format!("event: prefill\ndata: {json}\n\n").into_bytes(),
        None => b"event: alive\ndata: {}\n\n".to_vec(),
    })
}

/// The progress a prefill event reports, or a keep-alive when it does not parse.
pub(crate) fn prefill_of(data: &str) -> StreamEvent {
    #[derive(serde::Deserialize)]
    struct Progress {
        done: u64,
        total: u64,
    }
    match serde_json::from_str::<Progress>(data) {
        Ok(p) => StreamEvent::Prefill {
            done: p.done,
            total: p.total,
        },
        Err(_) => StreamEvent::Alive,
    }
}

/// The error a server sent inside the stream, with its own message.
/// The message is read from an object's `message`, or taken as the string sent.
/// Any other shape is quoted whole.
pub(crate) fn stream_failure(error: &serde_json::Value) -> anyhow::Error {
    let message = match error {
        serde_json::Value::String(s) => s.clone(),
        other => other
            .get("message")
            .and_then(|m| m.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| other.to_string()),
    };
    anyhow!("the server failed during the answer: {message}")
}

/// Everything one chunk has to say.
fn events_of(
    c: ChatChunk,
    think: &mut crate::backend::reasoning::ThinkSplitter,
) -> Vec<StreamEvent> {
    let mut out = Vec::new();
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut finish_reason = None;

    for choice in c.choices {
        // A server that splits the scratchpad out for us, under either name it might use.
        if let Some(s) = choice.delta.reasoning.or(choice.delta.reasoning_content) {
            reasoning.push_str(&s);
        }
        if let Some(s) = choice.delta.content {
            // Otherwise the tags arrive inside the content, a few characters at a time.
            // That makes this a state machine rather than a search.
            let split = think.push(&s);
            text.push_str(&split.answer);
            reasoning.push_str(&split.reasoning);
        }
        for call in choice.delta.tool_calls.into_iter().flatten() {
            out.push(StreamEvent::Call(call));
        }
        if choice.finish_reason.is_some() {
            finish_reason = choice.finish_reason;
        }
    }
    if !text.is_empty() {
        out.insert(0, StreamEvent::Delta(text));
    }
    if !reasoning.is_empty() {
        out.insert(0, StreamEvent::Reasoning(reasoning));
    }
    // The usage report arrives on a chunk of its own, after the one that ends generation.
    // An end is reported whenever either shows up.
    if finish_reason.is_some() || c.usage.is_some() {
        // Generation is over.
        // Anything the splitter still holds will never be disambiguated by a later fragment.
        // It is released rather than swallowed.
        let last = think.finish();
        if !last.answer.is_empty() {
            out.push(StreamEvent::Delta(last.answer));
        }
        if !last.reasoning.is_empty() {
            out.push(StreamEvent::Reasoning(last.reasoning));
        }
        out.push(StreamEvent::End {
            finish_reason,
            usage: c.usage,
        });
    }
    out
}

/// Whether a failure is worth another attempt.
enum Retry {
    Transient(anyhow::Error),
    Fatal(anyhow::Error),
}

/// A request that never reached a server at all.
///
/// Refusing to connect is the most retryable failure there is.
/// It is what a backend that is still starting up looks like from here.
fn classify_transport(e: reqwest::Error, url: &str) -> Retry {
    let wrapped = anyhow::Error::new(e);
    let transient = wrapped
        .downcast_ref::<reqwest::Error>()
        .is_some_and(|e| e.is_connect() || e.is_timeout() || e.is_request());
    let wrapped = wrapped.context(format!("POST {url}"));
    if transient {
        Retry::Transient(wrapped)
    } else {
        Retry::Fatal(wrapped)
    }
}

/// A request the server refused is only worth repeating if the refusal was about the
/// server's condition rather than about the request.
fn classify(status: reqwest::StatusCode, body: &str) -> Retry {
    let e = anyhow!("backend {status}: {body}");
    if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        Retry::Transient(e)
    } else {
        Retry::Fatal(e)
    }
}

/// Marker put on the error when a backend rejects native tool calls.
/// The agent recognises it after the error has travelled and drops to the envelope protocol.
pub const UNSUPPORTED_NATIVE_TOOLS: &str = "backend does not support native tool calls";

/// Whether a failure names one of the fields we could stop sending.
///
/// Matched on the text, which is all a rejected request gives back.
/// The reason is prose in a body, and every implementation words it differently.
fn mentions(e: &anyhow::Error, needles: &[&str]) -> bool {
    let text = format!("{e:#}");
    needles.iter().any(|n| text.contains(n))
}

/// The OpenAI-compatible wire format, which is what almost every server speaks.
///
/// LLMuse, ollama, vLLM, llama.cpp, LM Studio, OpenAI itself and the hosted services that
/// copied it all land here.
/// Servers differ in dialect, meaning which optional fields they accept.
/// Its capabilities carry the dialect.
///
/// See [`crate::backend::provider::Capabilities`] for what it declares it can do.
#[async_trait::async_trait]
impl crate::backend::provider::Provider for BackendClient {
    fn served_window(&self) -> Option<usize> {
        BackendClient::served_window(self)
    }

    fn label(&self) -> &str {
        "openai-compatible"
    }

    fn capabilities(&self) -> crate::backend::provider::Capabilities {
        crate::backend::provider::Capabilities {
            native_tools: true,
            grammar: true,
            // Learned rather than declared.
            // The client stops asking for usage once a server has rejected the field.
            // This reports what is actually happening.
            usage_in_stream: self.usage_supported.load(Ordering::Relaxed),
        }
    }

    async fn complete(&self, req: crate::backend::provider::TurnRequest) -> Result<String> {
        let purpose = req.purpose;
        self.complete_request(self.render(req), purpose).await
    }

    async fn stream(&self, req: crate::backend::provider::TurnRequest) -> Result<ChunkStream> {
        let purpose = req.purpose;
        self.stream_request(self.render(req), purpose).await
    }

    /// One token, which is what makes the server read the weights.
    ///
    /// There is no endpoint for "load this".
    /// Every server loads on the first request.
    /// The smallest possible request asks for it.
    /// Its answer is thrown away.
    async fn warm(&self, model: &str) -> Result<()> {
        let req = ChatCompletionRequest {
            model: model.to_string(),
            messages: vec![crate::backend::wire::Message::user("hi")],
            max_tokens: Some(1),
            temperature: Some(0.0),
            stream: Some(false),
            ..Default::default()
        };
        self.complete_request(req, Purpose::Probe).await.map(|_| ())
    }

    async fn models(&self) -> Result<Vec<String>> {
        let url = crate::backend::presets::endpoint(&self.base, "models");
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("{url} answered {status}");
        }
        let body: serde_json::Value = serde_json::from_str(&body).context("decoding /v1/models")?;
        Ok(model_ids(&body))
    }
}

/// The widest window a models listing states for `model`, `max_model_len` in its row.
fn model_window(body: &serde_json::Value, model: &str) -> Option<usize> {
    body.get("data")?
        .as_array()?
        .iter()
        .find(|row| row.get("id").and_then(|v| v.as_str()) == Some(model))?
        .get("max_model_len")?
        .as_u64()
        .map(|n| n as usize)
}

/// The ids out of a models listing, in the order the server gave them.
///
/// Both shapes are read: the OpenAI `data` array, and the bare array a few local servers
/// answer with.
fn model_ids(body: &serde_json::Value) -> Vec<String> {
    let rows = body
        .get("data")
        .and_then(|d| d.as_array())
        .or_else(|| body.as_array());
    rows.map(|rows| {
        rows.iter()
            .filter_map(|m| {
                m.get("id")
                    .or_else(|| m.get("name"))
                    .and_then(|v| v.as_str())
                    .or_else(|| m.as_str())
            })
            .map(str::to_string)
            .collect()
    })
    .unwrap_or_default()
}

impl BackendClient {
    /// Turn a neutral request into this dialect's body.
    fn render(&self, req: crate::backend::provider::TurnRequest) -> ChatCompletionRequest {
        let native = req.tools.is_some();
        ChatCompletionRequest {
            model: req.model,
            messages: req.messages,
            stream: Some(true),
            temperature: Some(req.temperature),
            top_p: None,
            max_tokens: Some(req.max_tokens),
            seed: None,
            response_format: req.grammar.map(|s| ResponseFormat::json_schema("turn", s)),
            user: req.user,
            stream_options: None,
            tools: req.tools,
            tool_choice: native.then(|| "auto".to_string()),
            reasoning_effort: (!req.reason && self.effort_supported.load(Ordering::Relaxed))
                .then(|| "minimal".to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    /// A chunk of comments alone is a keep-alive.
    /// One that carries data, or nothing, is not.
    #[test]
    fn a_comment_only_chunk_is_a_keep_alive() {
        assert!(super::is_keep_alive(b":\n\n"));
        assert!(super::is_keep_alive(b": keep-alive\n\n:\n\n"));
        assert!(!super::is_keep_alive(b"data: {}\n\n"));
        assert!(!super::is_keep_alive(b":\n\ndata: {}\n\n"));
        assert!(!super::is_keep_alive(b""));
    }

    /// A question beside the turn asks the server for as little reasoning as it allows.
    /// The turn itself asks nothing of the kind.
    #[test]
    fn a_side_question_asks_for_no_reasoning() {
        let c = BackendClient::new("http://127.0.0.1:1");
        let req = |reason| crate::backend::provider::TurnRequest {
            model: "m".into(),
            messages: Vec::new(),
            tools: None,
            grammar: None,
            max_tokens: 8,
            temperature: 0.0,
            user: None,
            reason,
            purpose: crate::backend::meter::Purpose::Step,
        };
        assert_eq!(
            c.render(req(false)).reasoning_effort.as_deref(),
            Some("minimal")
        );
        assert_eq!(c.render(req(true)).reasoning_effort, None);
    }

    /// The widest window of a model is read from its row of the model list, and only from
    /// its own.
    #[test]
    fn the_window_is_read_from_the_model_list() {
        let body = serde_json::json!({"object": "list", "data": [
            {"id": "a", "max_model_len": 32768},
            {"id": "b", "max_model_len": null},
            {"id": "c"},
        ]});
        assert_eq!(model_window(&body, "a"), Some(32768));
        assert_eq!(model_window(&body, "b"), None);
        assert_eq!(model_window(&body, "c"), None);
        assert_eq!(model_window(&body, "d"), None);
        assert_eq!(model_window(&serde_json::json!([]), "a"), None);
    }

    use super::*;
    use reqwest::StatusCode;

    /// A model's own mistake is not fixed by asking again.
    #[test]
    fn a_client_error_is_not_retried() {
        assert!(matches!(
            classify(StatusCode::BAD_REQUEST, "unknown field"),
            Retry::Fatal(_)
        ));
        assert!(matches!(
            classify(StatusCode::NOT_FOUND, "no such model"),
            Retry::Fatal(_)
        ));
    }

    /// A server that is briefly unwell, or busy, is exactly the case retries are for.
    #[test]
    fn server_trouble_and_rate_limits_are_retried() {
        assert!(matches!(
            classify(StatusCode::INTERNAL_SERVER_ERROR, ""),
            Retry::Transient(_)
        ));
        assert!(matches!(
            classify(StatusCode::SERVICE_UNAVAILABLE, ""),
            Retry::Transient(_)
        ));
        assert!(matches!(
            classify(StatusCode::TOO_MANY_REQUESTS, ""),
            Retry::Transient(_)
        ));
    }

    /// The signal that tells us to stop asking for usage has to survive being wrapped in
    /// the context the request path adds.
    #[test]
    fn a_complaint_about_stream_options_is_recognised() {
        let e = anyhow!("backend 400 Bad Request: unknown field `stream_options`")
            .context("POST http://x/v1/chat/completions");
        assert!(mentions(&e, &["stream_options", "include_usage"]));

        let unrelated = anyhow!("backend 400 Bad Request: model not found");
        assert!(!mentions(&unrelated, &["stream_options", "include_usage"]));
    }

    /// The measurements a loken server adds to the closing usage are read, each one optional.
    /// The standard speculation counts are read with them.
    /// Times written as whole numbers are read as well.
    #[test]
    fn a_loken_usage_is_read_off_the_last_chunk() {
        let raw = r#"{"choices":[],"usage":{"prompt_tokens":20431,"completion_tokens":120,
            "prompt_tokens_details":{"cached_tokens":8000},
            "completion_tokens_details":{"accepted_prediction_tokens":82,"rejected_prediction_tokens":18},
            "loken":{"queue_ms":12,"prefill_ms":840,"decode_ms":3120,
            "energy":{"joules":41.2,"gpu_joules":33.0,"cpu_joules":null,"estimated":false,"share":"work"},
            "kv":{"used_tokens":20431,"capacity_tokens":131072}}}}"#;
        let chunk: ChatChunk = serde_json::from_str(raw).expect("a chunk");
        let usage = chunk.usage.expect("a usage");
        let loken = usage.loken.expect("loken measurements");
        assert_eq!(loken.prefill_ms, Some(840.0));
        let energy = loken.energy.expect("an energy");
        assert_eq!(energy.share, crate::backend::wire::Share::Work);
        assert_eq!(energy.cpu_joules, None);
        assert_eq!(loken.kv.map(|k| k.capacity_tokens), Some(131_072));
        assert_eq!(
            usage.prompt_tokens_details.map(|d| d.cached_tokens),
            Some(8000)
        );
        assert_eq!(
            usage
                .completion_tokens_details
                .map(|d| d.accepted_prediction_tokens),
            Some(82)
        );
        // Any other server says none of it, and the usage still reads.
        let plain: ChatChunk = serde_json::from_str(
            r#"{"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":1}}"#,
        )
        .expect("a chunk");
        assert!(plain.usage.expect("a usage").loken.is_none());
    }

    /// The times a loken server sends are milliseconds with a fraction, and they are read
    /// as sent.
    #[test]
    fn a_loken_usage_with_fractional_times_is_read() {
        let raw = r#"{"choices":[],"usage":{"prompt_tokens":1830,"completion_tokens":64,
            "loken":{"queue_ms":1.2,"prefill_ms":840.5,"decode_ms":2210.0,
            "energy":{"joules":95.3,"gpu_joules":80.1,"cpu_joules":15.2,"estimated":false,"share":"work"},
            "kv":{"used_tokens":1830,"capacity_tokens":32768}}}}"#;
        let chunk: ChatChunk = serde_json::from_str(raw).expect("a chunk");
        let loken = chunk
            .usage
            .and_then(|u| u.loken)
            .expect("loken measurements");
        assert_eq!(loken.queue_ms, Some(1.2));
        assert_eq!(loken.prefill_ms, Some(840.5));
        assert_eq!(loken.decode_ms, Some(2210.0));
        assert_eq!(loken.energy.and_then(|e| e.cpu_joules), Some(15.2));
        assert_eq!(loken.kv.map(|k| k.used_tokens), Some(1830));
    }

    /// An error sent inside the stream keeps the server's message, whatever its shape.
    #[test]
    fn an_error_chunk_keeps_the_server_message() {
        let chunk: ChatChunk = serde_json::from_str(
            r#"{"error":{"message":"STREAMING PREFILL: out of memory","type":"server_error"}}"#,
        )
        .expect("a chunk");
        let error = chunk.error.expect("an error");
        assert!(stream_failure(&error)
            .to_string()
            .ends_with(": STREAMING PREFILL: out of memory"));
        let bare = serde_json::json!("model unloaded");
        assert!(stream_failure(&bare)
            .to_string()
            .ends_with(": model unloaded"));
        let odd = serde_json::json!({"code": 500});
        assert!(stream_failure(&odd).to_string().contains(r#"{"code":500}"#));
        // An ordinary chunk carries none.
        let plain: ChatChunk =
            serde_json::from_str(r#"{"choices":[],"error":null}"#).expect("a chunk");
        assert!(plain.error.is_none());
    }

    /// The comment that says how far a prompt has been read becomes an event of its own.
    /// Any other comment is a keep-alive.
    /// A chunk with data is left alone.
    #[test]
    fn a_prefill_comment_is_an_event_of_its_own() {
        let event = comment_event(b": loken-prefill {\"done\": 8192, \"total\": 20431}\n\n")
            .expect("a comment chunk");
        let text = String::from_utf8(event).expect("text");
        assert!(text.starts_with("event: prefill\n"), "{text}");
        let data = text
            .lines()
            .nth(1)
            .and_then(|l| l.strip_prefix("data: "))
            .expect("data");
        assert!(matches!(
            prefill_of(data),
            StreamEvent::Prefill {
                done: 8192,
                total: 20431
            }
        ));
        let alive = String::from_utf8(comment_event(b": keep-alive\n\n").expect("a comment"))
            .expect("text");
        assert!(alive.starts_with("event: alive\n"), "{alive}");
        assert!(comment_event(b"data: {}\n\n").is_none());
        assert!(matches!(prefill_of("not json"), StreamEvent::Alive));
    }
}
