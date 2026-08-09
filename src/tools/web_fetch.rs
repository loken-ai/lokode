use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::sandbox::approval::{self, ApprovalDecision, ApprovalRequest};
use crate::tools::registry::{Tool, ToolCtx, ToolOutput};


pub struct WebFetch;

#[derive(Deserialize)]
struct Args {
    url: String,
}

#[async_trait]
impl Tool for WebFetch {
    fn name(&self) -> &'static str { "web_fetch" }
    fn description(&self) -> &'static str {
        "GET a URL and return the response body (text). Requires user approval (network egress)."
    }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"url": {"type": "string", "format": "uri"}},
            "required": ["url"],
            "additionalProperties": false
        })
    }

    async fn run(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput> {
        let a: Args = serde_json::from_value(args)?;
        // Only http(s) — refuse file://, ftp://, etc.
        if !(a.url.starts_with("http://") || a.url.starts_with("https://")) {
            return Ok(ToolOutput::err("only http(s) URLs are allowed"));
        }

        if !ctx.session_approvals.is_always(self.name()).await {
            let decision = approval::ask(
                &ctx.approval,
                ApprovalRequest {
                    tool: self.name().into(),
                    summary: format!("GET {}", a.url),
                    diff_preview: None,
                },
            )
            .await;
            match decision {
                ApprovalDecision::Deny => return Ok(ToolOutput::err("user denied")),
                ApprovalDecision::Always => ctx.session_approvals.mark_always(self.name()).await,
                ApprovalDecision::Once => {}
            }
        }

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent("verve/0.1")
            .build()?;
        let resp = match client.get(&a.url).send().await {
            Ok(r) => r,
            Err(e) => return Ok(ToolOutput::err(format!("request failed: {e}"))),
        };
        let status = resp.status();
        let text = match resp.text().await {
            Ok(s) => s,
            Err(e) => return Ok(ToolOutput::err(format!("read body failed: {e}"))),
        };
        let body = ctx.clamp(strip_html(&text));
        Ok(ToolOutput::ok(format!("status: {status}\n\n{body}")))
    }
}

/// Strip naive HTML tags and collapse whitespace. Not a full DOM parser
/// — sufficient for "let the model read this page".
fn strip_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;
    for c in input.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    // Collapse runs of whitespace
    let mut collapsed = String::with_capacity(out.len());
    let mut prev_space = false;
    for c in out.chars() {
        if c.is_whitespace() {
            if !prev_space {
                collapsed.push(' ');
                prev_space = true;
            }
        } else {
            collapsed.push(c);
            prev_space = false;
        }
    }
    collapsed
}
