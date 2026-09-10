use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use eventsource_stream::Eventsource;
use futures::stream::{Stream, StreamExt};

use crate::backend::wire::{
    ChatChunk, ChatCompletionRequest, ResponseFormat, StreamOptions, ToolCallDelta, Usage,
};

/// What a streaming response hands back as it arrives.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    Delta(String),
    /// A piece of the model's scratchpad, however the server chose to mark it.
    ///
    /// Shown and then dropped: it never enters the conversation, because it is not what
    /// the model said - it is what it was working through on the way to saying it.
    Reasoning(String),
    /// A fragment of a native tool call, to be reassembled by index.
    Call(ToolCallDelta),
    /// The end of generation, carrying what the server was willing to say about it.
    End {
        finish_reason: Option<String>,
        usage: Option<Usage>,
    },
}

/// Raised when a server will not take something the request asked for.
///
/// Distinguished from an ordinary refusal so the caller can drop the feature and try
/// again rather than give up on the turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unsupported {
    Usage,
    NativeTools,
}

pub type ChunkStream = Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>;

/// How many times to try again after a failure that looks temporary.
const MAX_ATTEMPTS: usize = 3;

/// Wait before the first retry; doubled each time after.
const BACKOFF_BASE: Duration = Duration::from_millis(400);

pub struct BackendClient {
    http: reqwest::Client,
    base: String,
    /// Cleared the first time the server rejects `stream_options`, so the cost of finding
    /// out is paid once per session rather than on every request.
    usage_supported: AtomicBool,
}

impl BackendClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self::with_headers(base_url, &[])
    }

    /// A client that sends `headers` with every request.
    ///
    /// Built into the client's defaults rather than added per call, so there is one place
    /// a credential can be attached and no request path that can forget it. Headers that
    /// will not encode are dropped with a warning naming the header - never its value,
    /// which is the thing that must not reach a log.
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
                .timeout(std::time::Duration::from_secs(900))
                .default_headers(map)
                .build()
                .expect("reqwest client"),
            base: base_url.into(),
            usage_supported: AtomicBool::new(true),
        }
    }

    fn url(&self) -> String {
        format!("{}/v1/chat/completions", self.base.trim_end_matches('/'))
    }

    /// Non-streaming chat completion: returns the assistant content as
    /// a single string. Convenience for repair round-trips.
    async fn complete_request(&self, mut req: ChatCompletionRequest) -> Result<String> {
        req.stream = Some(false);
        req.stream_options = None;
        let url = self.url();

        let text = self
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
            .await?;

        let body: serde_json::Value =
            serde_json::from_str(&text).with_context(|| "decoding chat completion response")?;
        Ok(body["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_string())
    }

    /// Streaming chat completion.
    ///
    /// Retries cover establishing the response only. Once bytes are flowing the answer is
    /// partly on screen and partly in the transcript, and starting again would duplicate
    /// what the user has already read.
    async fn stream_request(&self, mut req: ChatCompletionRequest) -> Result<ChunkStream> {
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
                // A server that does not know the field says so with a 4xx. Give up the
                // token counts rather than the request.
                Err(e) if asked_for_usage && mentions(&e, &["stream_options", "include_usage"]) => {
                    tracing::warn!(
                        why = %e,
                        "backend will not take stream_options; giving up token counts for this session"
                    );
                    self.usage_supported.store(false, Ordering::Relaxed);
                    continue;
                }
                // Not something to work around here: the caller has to change protocol,
                // which means changing the system prompt and how the answer is read.
                Err(e) if req.tools.is_some() && mentions(&e, &["tools", "tool_choice"]) => {
                    tracing::warn!(why = %e, "backend will not take native tool calls");
                    return Err(e.context(UNSUPPORTED_NATIVE_TOOLS));
                }
                Err(e) => return Err(e),
            }
        };

        // One splitter per response: the tags it tracks can straddle chunk boundaries, so
        // the state has to outlive a single chunk and must not outlive the stream.
        let sse = resp.bytes_stream().eventsource();
        // One chunk can carry several things at once - a piece of prose, fragments of more
        // than one tool call, and the reason generation ended - so a chunk becomes a list
        // of events rather than one.
        // One splitter per response, moved into the closure: the tags it tracks can
        // straddle chunk boundaries, so the state must outlive a single chunk - and must
        // not outlive the stream, or one response's half-open tag would corrupt the next.
        let mut think = crate::backend::reasoning::ThinkSplitter::default();
        let mapped = sse.flat_map(move |ev| {
            let events: Vec<Result<StreamEvent>> = match ev {
                Err(e) => vec![Err(anyhow!("sse: {e}"))],
                Ok(event) if event.data.trim() == "[DONE]" => {
                    let _ = event;
                    Vec::new()
                }
                Ok(event) => match serde_json::from_str::<ChatChunk>(&event.data) {
                    Ok(c) => events_of(c, &mut think).into_iter().map(Ok).collect(),
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
            // And a server that does not: the tags arrive inside the content, a few
            // characters at a time, so this is a state machine rather than a search.
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
    // The usage report arrives on a chunk of its own, after the one that ends generation,
    // so an end is reported whenever either shows up.
    if finish_reason.is_some() || c.usage.is_some() {
        // Generation is over, so anything the splitter is still holding will never be
        // disambiguated by a later fragment - release it rather than swallow it.
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
/// Refusing to connect is the most retryable failure there is - it is what a backend that
/// is still starting up looks like from here, and it was the one case the first version
/// of this treated as final.
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

/// Marker put on the error when a backend rejects native tool calls, so the agent can
/// recognise it after the error has travelled and drop to the envelope protocol.
pub const UNSUPPORTED_NATIVE_TOOLS: &str = "backend does not support native tool calls";

/// Whether a failure names one of the fields we could stop sending.
///
/// Matched on the text because that is all a rejected request gives back - the reason is
/// prose in a body, and every implementation words it differently.
fn mentions(e: &anyhow::Error, needles: &[&str]) -> bool {
    let text = format!("{e:#}");
    needles.iter().any(|n| text.contains(n))
}

/// The OpenAI-compatible wire format, which is what almost every server speaks.
///
/// LLMuse, ollama, vLLM, llama.cpp, LM Studio, OpenAI itself and the hosted services that
/// copied it all land here. What differs between them is dialect - which optional fields
/// they accept - and that is what its capabilities carry, not a separate implementation
/// each.
///
/// See [`crate::backend::provider::Capabilities`] for what it declares it can do.
#[async_trait::async_trait]
impl crate::backend::provider::Provider for BackendClient {
    fn label(&self) -> &str {
        "openai-compatible"
    }

    fn capabilities(&self) -> crate::backend::provider::Capabilities {
        crate::backend::provider::Capabilities {
            native_tools: true,
            grammar: true,
            // Learned rather than declared: the client stops asking for usage once a
            // server has rejected the field, so this reports what is actually happening.
            usage_in_stream: self.usage_supported.load(Ordering::Relaxed),
        }
    }

    async fn complete(&self, req: crate::backend::provider::TurnRequest) -> Result<String> {
        self.complete_request(self.render(req)).await
    }

    async fn stream(&self, req: crate::backend::provider::TurnRequest) -> Result<ChunkStream> {
        self.stream_request(self.render(req)).await
    }

    /// One token, which is what makes the server read the weights.
    ///
    /// There is no endpoint for "load this": every server loads on the first request, so
    /// the smallest possible request is the way to ask. Its answer is thrown away.
    async fn warm(&self, model: &str) -> Result<()> {
        let req = ChatCompletionRequest {
            model: model.to_string(),
            messages: vec![crate::backend::wire::Message::user("hi")],
            max_tokens: Some(1),
            temperature: Some(0.0),
            stream: Some(false),
            ..Default::default()
        };
        self.complete_request(req).await.map(|_| ())
    }

    async fn models(&self) -> Result<Vec<String>> {
        let url = format!("{}/v1/models", self.base.trim_end_matches('/'));
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

/// The ids out of a models listing, in the order the server gave them.
///
/// Both shapes seen in the wild are read: the OpenAI `data` array, and the bare array a
/// few local servers answer with.
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
        }
    }
}

#[cfg(test)]
mod tests {
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
}
