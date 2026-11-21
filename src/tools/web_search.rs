//! A tool to search the web using a configured provider.
//!
//! The configuration is optional: the tool is only registered when a provider is set.
//! Supported providers: searxng, brave, tavily, exa, kagi.

use anyhow::Result;
use async_trait::async_trait;
use reqwest::Method;
use serde_json::{json, Value};
use url::Url;

use crate::sandbox::approval::{Approval, ApprovalRequest, ApprovalScope};
use crate::tools::registry::{Tool, ToolCtx, ToolOutput};

/// A web search provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Searxng,
    Brave,
    Tavily,
    Exa,
    Kagi,
}

impl Provider {
    /// Parse a provider name from the configuration.
    ///
    /// Lowercase names only. Returns `None` for unknown names.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "searxng" => Some(Provider::Searxng),
            "brave" => Some(Provider::Brave),
            "tavily" => Some(Provider::Tavily),
            "exa" => Some(Provider::Exa),
            "kagi" => Some(Provider::Kagi),
            _ => None,
        }
    }
}

/// The resolved configuration for a web search provider.
#[derive(Debug, Clone)]
pub struct SearchConfig {
    pub provider: Provider,
    pub url: Option<String>,
    pub key: Option<String>,
}

/// A web search result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// An HTTP request for a web search.
#[derive(Debug)]
pub struct Request {
    pub method: Method,
    pub url: Url,
    pub headers: Vec<(&'static str, String)>,
    pub body: Option<Value>,
}

/// The HTTP request that asks `cfg`'s provider for `query`.
///
/// The query goes into a URL through `query_pairs_mut`, never by string formatting.
/// A count is sent only when one is given: otherwise the provider's own default holds.
pub fn request(cfg: &SearchConfig, query: &str, count: Option<usize>) -> Result<Request, String> {
    let key = || {
        cfg.key
            .clone()
            .ok_or_else(|| format!("{:?} needs an api key", cfg.provider))
    };
    let fixed = |url: &str| Url::parse(url).map_err(|e| format!("invalid url {url}: {e}"));
    // The JSON body of a POST provider: the query, and the count under the provider's name.
    let body = |count_field: &str| {
        let mut body = json!({"query": query});
        if let Some(n) = count {
            body[count_field] = n.into();
        }
        body
    };
    let req = match cfg.provider {
        Provider::Searxng => {
            let base = cfg.url.as_deref().ok_or("searxng needs a url")?;
            let mut url = Url::parse(&format!("{}/search", base.trim_end_matches('/')))
                .map_err(|e| format!("invalid searxng url: {e}"))?;
            url.query_pairs_mut()
                .append_pair("q", query)
                .append_pair("format", "json");
            Request {
                method: Method::GET,
                url,
                headers: Vec::new(),
                body: None,
            }
        }
        Provider::Brave => {
            let mut url = fixed("https://api.search.brave.com/res/v1/web/search")?;
            {
                let mut pairs = url.query_pairs_mut();
                pairs.append_pair("q", query);
                if let Some(n) = count {
                    pairs.append_pair("count", &n.to_string());
                }
            }
            Request {
                method: Method::GET,
                url,
                headers: vec![
                    ("Accept", "application/json".to_string()),
                    ("X-Subscription-Token", key()?),
                ],
                body: None,
            }
        }
        Provider::Tavily => Request {
            method: Method::POST,
            url: fixed("https://api.tavily.com/search")?,
            headers: vec![("Authorization", format!("Bearer {}", key()?))],
            body: Some(body("max_results")),
        },
        Provider::Exa => {
            let mut b = body("numResults");
            b["contents"] = json!({"highlights": true});
            Request {
                method: Method::POST,
                url: fixed("https://api.exa.ai/search")?,
                headers: vec![("x-api-key", key()?)],
                body: Some(b),
            }
        }
        Provider::Kagi => Request {
            method: Method::POST,
            url: fixed("https://kagi.com/api/v1/search")?,
            headers: vec![("Authorization", format!("Bearer {}", key()?))],
            body: Some(body("limit")),
        },
    };
    Ok(req)
}

/// Parse search results from a JSON response body.
///
/// The expected result list path differs by provider:
/// - searxng, brave (with count), tavily, exa: `results`
/// - brave: `web.results`
/// - kagi: `data.search`
///
/// A missing result list path is an error naming the expected path.
/// Results without a `url` field are skipped.
/// Missing `title` or `snippet` fields become empty strings.
pub fn parse(provider: Provider, body: &Value) -> Result<Vec<Hit>, String> {
    let results = match provider {
        Provider::Searxng => body
            .get("results")
            .ok_or_else(|| "expected results list".to_string())?,
        Provider::Brave => body
            .get("web")
            .and_then(|w| w.get("results"))
            .ok_or_else(|| "expected web.results list".to_string())?,
        Provider::Tavily => body
            .get("results")
            .ok_or_else(|| "expected results list".to_string())?,
        Provider::Exa => body
            .get("results")
            .ok_or_else(|| "expected results list".to_string())?,
        Provider::Kagi => body
            .get("data")
            .and_then(|d| d.get("search"))
            .ok_or_else(|| "expected data.search list".to_string())?,
    };

    let mut hits = Vec::new();
    for item in results.as_array().ok_or("expected a list")?.iter() {
        let url = item
            .get("url")
            .and_then(|u| u.as_str())
            .map(|s| s.to_string());
        // Skip results without a URL.
        let url = match url {
            Some(u) => u,
            None => continue,
        };

        let title = item
            .get("title")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();

        let snippet = match provider {
            Provider::Searxng => item
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string(),
            Provider::Brave => item
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("")
                .to_string(),
            Provider::Tavily => item
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string(),
            Provider::Exa => {
                // Join highlights with " ... ", fall back to text, then empty.
                item.get("highlights")
                    .and_then(|h| h.as_array())
                    .map(|h| {
                        h.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(" ... ")
                    })
                    .filter(|s| !s.is_empty())
                    .or_else(|| {
                        item.get("text")
                            .and_then(|t| t.as_str().map(|s| s.to_string()))
                    })
                    .unwrap_or_default()
            }
            Provider::Kagi => item
                .get("snippet")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string(),
        };

        hits.push(Hit {
            title,
            url,
            snippet,
        });
    }

    Ok(hits)
}

/// A web search tool.
pub struct WebSearch {
    pub config: SearchConfig,
}

#[async_trait]
impl Tool for WebSearch {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web with the configured provider and return the results: title, URL and a short text for each. Requires user approval (network egress). Read a result with web_fetch."
    }

    fn args_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Search query"},
                "count": {"type": "integer", "description": "Number of results (at least 1)", "minimum": 1}
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    async fn run(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing or invalid query parameter"))?;

        let count = match args.get("count") {
            None => None,
            Some(v) => match v.as_u64() {
                Some(n) => Some(n as usize),
                None => {
                    return Ok(ToolOutput::err(
                        "count must be a whole number of at least 1",
                    ))
                }
            },
        };

        // Empty query is an error.
        if query.trim().is_empty() {
            return Ok(ToolOutput::err("query cannot be empty".to_string()));
        }

        // Count of 0 or below is an error.
        let count = match count {
            Some(0) => return Ok(ToolOutput::err("count must be at least 1".to_string())),
            Some(n) => Some(n),
            None => None,
        };

        let req = match request(&self.config, query, count) {
            Ok(r) => r,
            Err(e) => return Ok(ToolOutput::err(format!("failed to build request: {e}"))),
        };
        let host = req.url.host_str().unwrap_or_default().to_string();

        match ctx
            .approve(ApprovalRequest {
                tool: self.name().into(),
                summary: format!("search \"{query}\" on {host}"),
                diff_preview: None,
                scope: ApprovalScope::Host(host.clone()),
            })
            .await
        {
            Approval::Proceed => {}
            Approval::Denied(why) => {
                return Ok(ToolOutput::err(crate::sandbox::approval::denial(why)));
            }
            Approval::Interrupted => return Ok(ToolOutput::err("interrupted by user")),
        };

        // Send the request, racing with ctx.cancel.
        let http = ctx.http.clone();
        let resp = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => return Ok(ToolOutput::err("interrupted by user")),
            r = send_request(&http, req) => r,
        };

        let resp = match resp {
            Ok(r) => r,
            Err(e) => return Ok(ToolOutput::err(format!("request failed: {e}"))),
        };

        let status = resp.status();
        if !status.is_success() {
            // Read some of the body for the error message.
            use futures::StreamExt;
            let mut stream = resp.bytes_stream();
            let mut bytes = Vec::new();
            loop {
                tokio::select! {
                    biased;
                    _ = ctx.cancel.cancelled() => break,
                    c = stream.next() => match c {
                        Some(Ok(b)) => {
                            bytes.extend_from_slice(&b);
                            if bytes.len() >= 300 {
                                break;
                            }
                        }
                        Some(Err(_)) => break,
                        None => break,
                    },
                }
            }
            let text_body = String::from_utf8_lossy(&bytes).into_owned();
            let mut err = format!("status: {status}");
            if !text_body.is_empty() {
                let head: String = text_body.chars().take(300).collect();
                err.push_str(&format!(": {head}"));
            }
            // Add a hint for 403 from searxng.
            if status.as_u16() == 403 && self.config.provider == Provider::Searxng {
                err.push_str(" (the instance may not allow format=json)");
            }
            return Ok(ToolOutput::err(err));
        }

        // Parse the response body as JSON and extract hits.
        let json_body: Value = match resp.json().await {
            Ok(j) => j,
            Err(e) => {
                return Ok(ToolOutput::err(format!(
                    "failed to parse response JSON: {e}"
                )))
            }
        };

        let hits = match parse(self.config.provider, &json_body) {
            Ok(h) => h,
            Err(e) => return Ok(ToolOutput::err(e)),
        };

        if hits.is_empty() {
            return Ok(ToolOutput::ok(format!("no results for \"{query}\"")));
        }

        // Format the results.
        let mut out = String::new();
        for (i, hit) in hits.iter().enumerate() {
            if i > 0 {
                out.push_str("\n\n");
            }
            out.push_str(&format!(
                "{}. {}\n  {}\n  {}",
                i + 1,
                hit.title,
                hit.url,
                hit.snippet
            ));
        }

        Ok(ToolOutput::ok(ctx.clamp(out)))
    }
}

/// Send a built request using reqwest.
async fn send_request(http: &reqwest::Client, req: Request) -> Result<reqwest::Response, String> {
    let mut builder = http.request(req.method, req.url);
    for (k, v) in req.headers {
        builder = builder.header(k, v);
    }
    if let Some(body) = req.body {
        builder = builder.json(&body);
    }
    builder.send().await.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_searxng() {
        let cfg = SearchConfig {
            provider: Provider::Searxng,
            url: Some("https://searxng.example.com/".to_string()),
            key: None,
        };
        let req = request(&cfg, "hello & world", None).unwrap();
        assert_eq!(req.method, Method::GET);
        assert_eq!(
            req.url.as_str(),
            "https://searxng.example.com/search?q=hello+%26+world&format=json"
        );
        // searxng has no special headers by default
        assert!(req.headers.is_empty());
    }

    #[test]
    fn request_searxng_no_trailing_slash() {
        let cfg = SearchConfig {
            provider: Provider::Searxng,
            url: Some("https://searxng.example.com///".to_string()),
            key: None,
        };
        let req = request(&cfg, "query", None).unwrap();
        assert_eq!(
            req.url.as_str(),
            "https://searxng.example.com/search?q=query&format=json"
        );
    }

    #[test]
    fn request_brave() {
        let cfg = SearchConfig {
            provider: Provider::Brave,
            url: None,
            key: Some("test-key".to_string()),
        };
        let req = request(&cfg, "hello & world", Some(10)).unwrap();
        assert_eq!(req.method, Method::GET);
        assert_eq!(
            req.url.as_str(),
            "https://api.search.brave.com/res/v1/web/search?q=hello+%26+world&count=10"
        );
        assert!(req
            .headers
            .iter()
            .any(|(k, v)| k == &"X-Subscription-Token" && v == "test-key"));
        assert_eq!(req.body, None);
    }

    #[test]
    fn request_tavily() {
        let cfg = SearchConfig {
            provider: Provider::Tavily,
            url: None,
            key: Some("test-key".to_string()),
        };
        let req = request(&cfg, "hello & world", Some(10)).unwrap();
        assert_eq!(req.method, Method::POST);
        assert_eq!(req.url.as_str(), "https://api.tavily.com/search");
        assert!(req
            .headers
            .iter()
            .any(|(k, v)| k == &"Authorization" && v == "Bearer test-key"));
        let body = req.body.unwrap();
        assert_eq!(body["query"], "hello & world");
        assert_eq!(body["max_results"], 10);
    }

    #[test]
    fn request_exa() {
        let cfg = SearchConfig {
            provider: Provider::Exa,
            url: None,
            key: Some("test-key".to_string()),
        };
        let req = request(&cfg, "hello & world", Some(10)).unwrap();
        assert_eq!(req.method, Method::POST);
        assert_eq!(req.url.as_str(), "https://api.exa.ai/search");
        assert!(req
            .headers
            .iter()
            .any(|(k, v)| k == &"x-api-key" && v == "test-key"));
        let body = req.body.unwrap();
        assert_eq!(body["query"], "hello & world");
        assert_eq!(body["numResults"], 10);
        assert_eq!(body["contents"]["highlights"], true);
    }

    #[test]
    fn request_kagi() {
        let cfg = SearchConfig {
            provider: Provider::Kagi,
            url: None,
            key: Some("test-key".to_string()),
        };
        let req = request(&cfg, "hello & world", Some(10)).unwrap();
        assert_eq!(req.method, Method::POST);
        assert_eq!(req.url.as_str(), "https://kagi.com/api/v1/search");
        assert!(req
            .headers
            .iter()
            .any(|(k, v)| k == &"Authorization" && v == "Bearer test-key"));
        let body = req.body.unwrap();
        assert_eq!(body["query"], "hello & world");
        assert_eq!(body["limit"], 10);
    }

    #[test]
    fn request_searxng_with_url_trailing_slash() {
        let cfg = SearchConfig {
            provider: Provider::Searxng,
            url: Some("https://searxng.example.com/".to_string()),
            key: None,
        };
        let req = request(&cfg, "query", None).unwrap();
        // Trailing slash should be stripped
        assert_eq!(
            req.url.as_str(),
            "https://searxng.example.com/search?q=query&format=json"
        );
    }

    #[test]
    fn parse_searxng() {
        let body = json!({
            "results": [
                {
                    "title": "First result",
                    "url": "https://example.com/1",
                    "content": "Snippet text"
                },
                {
                    "title": "Second result",
                    "url": "https://example.com/2",
                    "content": "Another snippet"
                }
            ]
        });
        let hits = parse(Provider::Searxng, &body).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "First result");
        assert_eq!(hits[0].url, "https://example.com/1");
        assert_eq!(hits[0].snippet, "Snippet text");
    }

    #[test]
    fn parse_searxng_without_snippet() {
        let body = json!({
            "results": [
                {
                    "title": "No snippet",
                    "url": "https://example.com/1"
                }
            ]
        });
        let hits = parse(Provider::Searxng, &body).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].snippet, "");
    }

    #[test]
    fn parse_brave() {
        let body = json!({
            "web": {
                "results": [
                    {
                        "title": "Brave result",
                        "url": "https://brave.example.com/1",
                        "description": "Brave snippet"
                    }
                ]
            }
        });
        let hits = parse(Provider::Brave, &body).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Brave result");
        assert_eq!(hits[0].snippet, "Brave snippet");
    }

    #[test]
    fn parse_tavily() {
        let body = json!({
            "results": [
                {
                    "title": "Tavily result",
                    "url": "https://tavily.example.com/1",
                    "content": "Tavily snippet"
                }
            ]
        });
        let hits = parse(Provider::Tavily, &body).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].snippet, "Tavily snippet");
    }

    #[test]
    fn parse_exa() {
        let body = json!({
            "results": [
                {
                    "title": "Exa result",
                    "url": "https://exa.example.com/1",
                    "highlights": ["highlight 1", "highlight 2"]
                }
            ]
        });
        let hits = parse(Provider::Exa, &body).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].snippet, "highlight 1 ... highlight 2");
    }

    #[test]
    fn parse_kagi() {
        let body = json!({
            "data": {
                "search": [
                    {
                        "title": "Kagi result",
                        "url": "https://kagi.example.com/1",
                        "snippet": "Kagi snippet"
                    }
                ]
            }
        });
        let hits = parse(Provider::Kagi, &body).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].snippet, "Kagi snippet");
    }

    #[test]
    fn parse_skips_result_without_url() {
        let body = json!({
            "results": [
                {
                    "title": "No URL result",
                    "content": "No URL here"
                },
                {
                    "title": "Has URL",
                    "url": "https://example.com/1",
                    "content": "Has URL"
                }
            ]
        });
        let hits = parse(Provider::Searxng, &body).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://example.com/1");
    }

    #[test]
    fn parse_missing_results_list() {
        let body = json!({
            "other_field": []
        });
        let err = parse(Provider::Searxng, &body).unwrap_err();
        assert!(err.contains("expected results list"));
    }

    #[test]
    fn parse_brave_missing_web_results() {
        let body = json!({
            "other": []
        });
        let err = parse(Provider::Brave, &body).unwrap_err();
        assert!(err.contains("expected web.results list"));
    }

    #[test]
    fn parse_kagi_missing_data_search() {
        let body = json!({
            "other": []
        });
        let err = parse(Provider::Kagi, &body).unwrap_err();
        assert!(err.contains("expected data.search list"));
    }

    #[test]
    fn parse_unknown_provider() {
        assert!(Provider::parse("google").is_none());
        assert!(Provider::parse("unknown-provider").is_none());
        assert!(Provider::parse("GOOGLE").is_none());
    }
}
