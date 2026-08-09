use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use eventsource_stream::Eventsource;
use futures::stream::{Stream, StreamExt};

use crate::backend::wire::{ChatChunk, ChatCompletionRequest, StreamOptions, Usage};

/// What a streaming response hands back as it arrives.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    Delta(String),
    /// The end of generation, carrying what the server was willing to say about it.
    End {
        finish_reason: Option<String>,
        usage: Option<Usage>,
    },
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
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(900))
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
    pub async fn complete(&self, mut req: ChatCompletionRequest) -> Result<String> {
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
    pub async fn stream(&self, mut req: ChatCompletionRequest) -> Result<ChunkStream> {
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
                Err(e) if asked_for_usage && mentions_stream_options(&e) => {
                    tracing::warn!(
                        why = %e,
                        "backend will not take stream_options; giving up token counts for this session"
                    );
                    self.usage_supported.store(false, Ordering::Relaxed);
                    continue;
                }
                Err(e) => return Err(e),
            }
        };

        let sse = resp.bytes_stream().eventsource();
        let mapped = sse.filter_map(|ev| async move {
            match ev {
                Err(e) => Some(Err(anyhow!("sse: {e}"))),
                Ok(event) => {
                    if event.data.trim() == "[DONE]" {
                        return None;
                    }
                    match serde_json::from_str::<ChatChunk>(&event.data) {
                        Ok(c) => {
                            let mut piece = String::new();
                            let mut finish_reason = None;
                            for choice in c.choices {
                                if let Some(s) = choice.delta.content {
                                    piece.push_str(&s);
                                }
                                if choice.finish_reason.is_some() {
                                    finish_reason = choice.finish_reason;
                                }
                            }
                            // The usage chunk carries no content and the chunk that ends
                            // generation may carry both, so an end is reported whenever
                            // either arrives and the deltas are kept separate.
                            if finish_reason.is_some() || c.usage.is_some() {
                                return Some(Ok(StreamEvent::End {
                                    finish_reason,
                                    usage: c.usage,
                                }));
                            }
                            if piece.is_empty() {
                                None
                            } else {
                                Some(Ok(StreamEvent::Delta(piece)))
                            }
                        }
                        Err(e) => Some(Err(anyhow!("decode chunk: {e}: raw={}", event.data))),
                    }
                }
            }
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

fn mentions_stream_options(e: &anyhow::Error) -> bool {
    let text = format!("{e:#}");
    text.contains("stream_options") || text.contains("include_usage")
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
        assert!(mentions_stream_options(&e));

        let unrelated = anyhow!("backend 400 Bad Request: model not found");
        assert!(!mentions_stream_options(&unrelated));
    }
}
