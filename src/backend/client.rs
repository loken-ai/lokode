use anyhow::{anyhow, Context, Result};
use eventsource_stream::Eventsource;
use futures::stream::{Stream, StreamExt};
use std::pin::Pin;

use crate::backend::wire::{ChatChunk, ChatCompletionRequest};

pub type ChunkStream = Pin<Box<dyn Stream<Item = Result<String>> + Send>>;

pub struct BackendClient {
    http: reqwest::Client,
    base: String,
}

impl BackendClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(900))
                .build()
                .expect("reqwest client"),
            base: base_url.into(),
        }
    }

    /// Non-streaming chat completion: returns the assistant content as
    /// a single string. Convenience for repair round-trips.
    pub async fn complete(&self, mut req: ChatCompletionRequest) -> Result<String> {
        req.stream = Some(false);
        let url = format!("{}/v1/chat/completions", self.base.trim_end_matches('/'));
        let resp = self
            .http
            .post(&url)
            .json(&req)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(anyhow!("backend {status}: {text}"));
        }
        let body: serde_json::Value = serde_json::from_str(&text)
            .with_context(|| "decoding chat completion response")?;
        Ok(body["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_string())
    }

    /// Streaming chat completion: returns a stream of content deltas
    /// (already concatenated server-side per chunk).
    pub async fn stream(&self, mut req: ChatCompletionRequest) -> Result<ChunkStream> {
        req.stream = Some(true);
        let url = format!("{}/v1/chat/completions", self.base.trim_end_matches('/'));
        let resp = self
            .http
            .post(&url)
            .json(&req)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("backend {status}: {text}"));
        }

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
                            for choice in c.choices {
                                if let Some(s) = choice.delta.content {
                                    piece.push_str(&s);
                                }
                            }
                            if piece.is_empty() { None } else { Some(Ok(piece)) }
                        }
                        Err(e) => Some(Err(anyhow!("decode chunk: {e}: raw={}", event.data))),
                    }
                }
            }
        });

        Ok(Box::pin(mapped))
    }
}
