//! The tools that ask a language server: what it reports about a file, and what it says
//! about the symbol at a position.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::lsp::LspPool;
use crate::lsp::{check, position};
use crate::sandbox::allowlist;
use crate::tools::registry::{Tool, ToolCtx, ToolOutput};

/// How many diagnostics the tool lists before it counts the rest.
const LISTED: usize = 50;

pub struct LspDiagnostics {
    pub pool: Arc<LspPool>,
}

#[derive(Deserialize)]
struct DiagArgs {
    path: PathBuf,
}

#[async_trait]
impl Tool for LspDiagnostics {
    fn name(&self) -> &str {
        "lsp_diagnostics"
    }
    fn description(&self) -> &str {
        "Return current LSP diagnostics (errors/warnings) for a source file."
    }
    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
            "additionalProperties": false
        })
    }
    async fn run(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput> {
        let a: DiagArgs = serde_json::from_value(args)?;
        let path = match allowlist::resolve(&ctx.root, &a.path, true) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::err(allowlist::refusal(&ctx.root, &a.path, &e))),
        };
        let server = match self.pool.server_for(&path).await {
            Ok(s) => s,
            Err(why) => return Ok(ToolOutput::err(why)),
        };
        let asked = std::time::Instant::now();
        let target = match server.sync(&path).await {
            Ok(t) => t,
            Err(e) => {
                return Ok(ToolOutput::err(format!(
                    "could not reach the language server: {e:#}"
                )))
            }
        };
        let settled = check::settle(
            &server,
            std::slice::from_ref(&target),
            asked,
            check::SETTLE_AT_MOST,
        )
        .await;
        if let (Err(why), false) = (&settled, server.alive()) {
            return Ok(ToolOutput::err(why.clone()));
        }
        let shown = ctx.short(&path);
        let Some(published) = server.published(&target.0).await else {
            return Ok(match settled {
                Ok(()) => ToolOutput::ok(format!("{shown}: no diagnostics")),
                Err(why) => ToolOutput::err(why),
            });
        };
        let mut diags = published.diagnostics;
        diags.sort_by_key(|d| {
            (
                d.severity.map(|s| format!("{s:?}")) != Some("Error".into()),
                d.range.start.line,
            )
        });
        let mut out = Vec::new();
        for d in diags.iter().take(LISTED) {
            out.push(check::describe(&server, &ctx.root, &target.0, d).await);
        }
        if diags.len() > LISTED {
            out.push(format!("and {} more", diags.len() - LISTED));
        }
        if out.is_empty() {
            out.push(format!("{shown}: no diagnostics"));
        }
        if let Err(why) = settled {
            out.push(format!("this may be incomplete: {why}"));
        }
        Ok(ToolOutput::ok(out.join("\n")))
    }
}

pub struct LspHover {
    pub pool: Arc<LspPool>,
}

#[derive(Deserialize)]
struct HoverArgs {
    path: PathBuf,
    /// 1-indexed line.
    line: u32,
    /// 1-indexed column, in characters.
    column: u32,
}

#[async_trait]
impl Tool for LspHover {
    fn name(&self) -> &str {
        "lsp_hover"
    }
    fn description(&self) -> &str {
        "Get type/hover info at a 1-indexed (line, column) in a source file."
    }
    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":   {"type": "string"},
                "line":   {"type": "integer", "minimum": 1},
                "column": {"type": "integer", "minimum": 1}
            },
            "required": ["path", "line", "column"],
            "additionalProperties": false
        })
    }
    async fn run(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput> {
        let a: HoverArgs = serde_json::from_value(args)?;
        let path = match allowlist::resolve(&ctx.root, &a.path, true) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::err(allowlist::refusal(&ctx.root, &a.path, &e))),
        };
        let server = match self.pool.server_for(&path).await {
            Ok(s) => s,
            Err(why) => return Ok(ToolOutput::err(why)),
        };
        // The line and column mean nothing against an older copy of the file.
        let uri = match server.sync(&path).await {
            Ok((uri, _, _)) => uri,
            Err(e) => {
                return Ok(ToolOutput::err(format!(
                    "could not reach the language server: {e:#}"
                )))
            }
        };
        let text = server.text_of(&path).await.unwrap_or_default();
        let line = a.line.saturating_sub(1);
        let character = position::units(
            text.lines().nth(line as usize).unwrap_or(""),
            a.column.saturating_sub(1) as usize,
            server.caps().encoding,
        );
        let params = json!({
            "textDocument": {"uri": uri},
            "position": {"line": line, "character": character}
        });
        let reply = match server.request("textDocument/hover", params).await {
            Ok(r) => r,
            Err(e) => return Ok(ToolOutput::err(format!("hover failed: {e:#}"))),
        };
        if reply.is_null() {
            return Ok(ToolOutput::ok("(no hover info)"));
        }
        let txt = reply
            .get("contents")
            .map(pretty_contents)
            .unwrap_or_else(|| reply.to_string());
        Ok(ToolOutput::ok(txt))
    }
}

fn pretty_contents(c: &Value) -> String {
    // Hover contents can be a string, a {kind, value}, or an array.
    if let Some(s) = c.as_str() {
        return s.to_string();
    }
    if let Some(v) = c.get("value").and_then(|v| v.as_str()) {
        return v.to_string();
    }
    if let Some(arr) = c.as_array() {
        return arr
            .iter()
            .map(pretty_contents)
            .collect::<Vec<_>>()
            .join("\n");
    }
    c.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::harness::{Answer, Harness};
    use serde_json::json;

    /// A language server that answers the parts of the protocol these two tools use.
    ///
    /// Written out by the test rather than kept as a fixture so it cannot drift from the
    /// test that runs it. It reports the document version back inside the diagnostic, which
    /// is what makes it possible to tell whether an edit actually reached the server -
    /// without that, a stale diagnostic and a fresh one look identical.
    const STUB: &str = r#"
import json, sys

def read():
    length = 0
    while True:
        line = sys.stdin.buffer.readline()
        if not line or line in (b"\r\n", b"\n"):
            break
        if line.lower().startswith(b"content-length:"):
            length = int(line.split(b":")[1])
    if not length:
        return None
    return json.loads(sys.stdin.buffer.read(length))

def send(msg):
    body = json.dumps(msg).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body))
    sys.stdout.buffer.write(body)
    sys.stdout.buffer.flush()

versions = {}
while True:
    msg = read()
    if msg is None:
        break
    method, mid = msg.get("method"), msg.get("id")
    if method == "initialize":
        # A request of the server's own, numbered like the client's first one: the client
        # has to answer it, and must not take it for the answer to its own request.
        send({"jsonrpc": "2.0", "id": mid, "method": "workspace/configuration",
              "params": {"items": [{"section": "stub"}]}})
        answer = read()
        config = answer.get("result") if answer and "method" not in answer else "unanswered"
        send({"jsonrpc": "2.0", "id": mid, "result": {"capabilities": {"hoverProvider": True}}})
    elif method in ("textDocument/didOpen", "textDocument/didChange"):
        doc = msg["params"]["textDocument"]
        text = doc.get("text") or msg["params"].get("contentChanges", [{}])[0].get("text", "")
        if "CRASH" in text:
            sys.stderr.write("stub crashed on purpose\n")
            sys.exit(1)
        versions[doc["uri"]] = doc.get("version", 0)
        send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {
            "uri": doc["uri"],
            "diagnostics": [{
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
                "severity": 1,
                "message": "stub saw version %d" % versions[doc["uri"]],
            }],
        }})
    elif method == "textDocument/hover":
        pos = msg["params"]["position"]
        send({"jsonrpc": "2.0", "id": mid, "result": {"contents": {"kind": "plaintext",
            "value": "stub hover at %d:%d, configured %s" % (pos["line"], pos["character"], json.dumps(config))}}})
    elif mid is not None:
        send({"jsonrpc": "2.0", "id": mid, "result": None})
"#;

    /// Point the Rust language server at the stub.
    ///
    /// The override is read from the environment when a server is spawned, and the
    /// environment belongs to the whole process - so these tests are the only ones that
    /// touch it, and they run one after another inside a single test function.
    fn install_stub(h: &Harness) -> bool {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            return false;
        }
        let script = h.path("stub_lsp.py");
        std::fs::write(&script, STUB).unwrap();
        std::env::set_var("LOKODE_LSP_RUST", format!("python3 {}", script.display()));
        true
    }

    /// One function, because they share the process environment and a language-server pool.
    #[tokio::test]
    async fn the_language_server_tools_talk_to_a_server() {
        let h = Harness::new(Answer::Yes);
        if !install_stub(&h) {
            eprintln!("skipping: python3 is not available");
            return;
        }
        h.write("a.rs", "fn main() {}\n");
        let pool = LspPool::new(h.root.clone());

        // Diagnostics come back from a server that was actually spoken to.
        let out = LspDiagnostics { pool: pool.clone() }
            .run(&h.ctx, json!({"path": "a.rs"}))
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("stub saw version"), "{}", out.content);

        // The file is edited behind the server's back, exactly as the agent's own write
        // tools do it. Asking again must describe the new text: the server used to be told
        // once and never again, so from the first edit every diagnostic described code that
        // no longer existed.
        let first = out.content.clone();
        h.write("a.rs", "fn main() { let x = 1; }\n");
        let after = LspDiagnostics { pool: pool.clone() }
            .run(&h.ctx, json!({"path": "a.rs"}))
            .await
            .unwrap();
        assert_ne!(
            first, after.content,
            "the server was not told the file had changed"
        );
        assert!(after.content.contains("version 2"), "{}", after.content);

        // Hover reaches the same server, which had its own request answered rather than
        // taken for an answer. The column is sent in UTF-16 units: the emoji counts two.
        h.write("a.rs", "// 🙂 x\n");
        let hover = LspHover { pool: pool.clone() }
            .run(&h.ctx, json!({"path": "a.rs", "line": 1, "column": 6}))
            .await
            .unwrap();
        assert!(!hover.is_error, "{}", hover.content);
        assert!(
            hover
                .content
                .contains("stub hover at 0:6, configured [null]"),
            "{}",
            hover.content
        );

        // A server that stops is started again once, and the second stop leaves it down
        // with what it said.
        h.write("a.rs", "CRASH\n");
        let crashed = LspDiagnostics { pool: pool.clone() }
            .run(&h.ctx, json!({"path": "a.rs"}))
            .await
            .unwrap();
        assert!(crashed.is_error, "{}", crashed.content);
        assert!(crashed.content.contains("stopped"), "{}", crashed.content);
        h.write("a.rs", "fn main() {}\n");
        let again = LspDiagnostics { pool: pool.clone() }
            .run(&h.ctx, json!({"path": "a.rs"}))
            .await
            .unwrap();
        assert!(
            again.content.contains("stub saw version 1"),
            "{}",
            again.content
        );
        h.write("a.rs", "CRASH again\n");
        let _ = LspDiagnostics { pool: pool.clone() }
            .run(&h.ctx, json!({"path": "a.rs"}))
            .await
            .unwrap();
        let down = LspDiagnostics { pool: pool.clone() }
            .run(&h.ctx, json!({"path": "a.rs"}))
            .await
            .unwrap();
        assert!(down.is_error, "{}", down.content);
        assert!(down.content.contains("left down"), "{}", down.content);
        assert!(
            down.content.contains("stub crashed on purpose"),
            "{}",
            down.content
        );

        // A path outside the root is refused before any of this is reached.
        let escaped = LspDiagnostics { pool: pool.clone() }
            .run(&h.ctx, json!({"path": "../outside.rs"}))
            .await
            .unwrap();
        assert!(escaped.is_error, "{}", escaped.content);

        pool.shutdown().await;
        std::env::remove_var("LOKODE_LSP_RUST");
    }
}
