//! A Model Context Protocol client, over a server's standard input and output.
//!
//! JSON-RPC again, but not framed the way the language server protocol frames it: one
//! message per line, no headers. Close enough to be confusing and different enough to
//! break if you assume.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{oneshot, Mutex};

/// The revision this client speaks. A server that wants a different one says so, and is
/// taken at its word rather than argued with.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// How long to wait for any one request.
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Bytes of a server's complaints to keep for when something goes wrong.
const STDERR_TAIL: usize = 4 * 1024;

/// What a server said it can do.
#[derive(Debug, Clone, Deserialize)]
pub struct McpToolInfo {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// JSON Schema for the arguments. Written by somebody else, so it is checked before it
    /// is allowed anywhere near the grammar the model is held to.
    #[serde(rename = "inputSchema", default)]
    pub input_schema: Value,
}

pub struct McpClient {
    /// Named by the configuration, and used to prefix its tools.
    pub server: String,
    _child: Child,
    stdin: Mutex<ChildStdin>,
    next_id: AtomicI64,
    pending: Mutex<HashMap<i64, oneshot::Sender<Value>>>,
    stderr: Arc<Mutex<String>>,
}

impl McpClient {
    /// Start a server and complete the handshake.
    pub async fn start(
        server: &str,
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
        cwd: &std::path::Path,
    ) -> Result<Arc<Self>> {
        let mut child = tokio::process::Command::new(command)
            .args(args)
            .envs(env)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| anyhow!("spawn {command}: {e}"))?;

        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;

        let stderr_buf = Arc::new(Mutex::new(String::new()));
        if let Some(stderr) = child.stderr.take() {
            let buf = stderr_buf.clone();
            tokio::spawn(async move { drain_stderr(stderr, buf).await });
        }

        let client = Arc::new(Self {
            server: server.to_string(),
            _child: child,
            stdin: Mutex::new(stdin),
            next_id: AtomicI64::new(1),
            pending: Mutex::new(HashMap::new()),
            stderr: stderr_buf,
        });

        {
            let client = client.clone();
            tokio::spawn(async move { reader_loop(client, stdout).await });
        }

        let init = client
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "verve", "version": env!("CARGO_PKG_VERSION") },
                }),
            )
            .await;
        if let Err(e) = init {
            return Err(anyhow!(
                "{server} did not initialise: {e}{}",
                client.stderr_tail().await
            ));
        }
        client
            .notify("notifications/initialized", json!({}))
            .await?;
        Ok(client)
    }

    /// What this server offers.
    pub async fn list_tools(&self) -> Result<Vec<McpToolInfo>> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None => json!({}),
            };
            let page = self.request("tools/list", params).await?;
            if let Some(tools) = page.get("tools").and_then(|t| t.as_array()) {
                for t in tools {
                    match serde_json::from_value::<McpToolInfo>(t.clone()) {
                        Ok(info) => out.push(info),
                        Err(e) => tracing::warn!(
                            server = %self.server,
                            why = %e,
                            "skipping a tool this server described in a way we cannot read"
                        ),
                    }
                }
            }
            // A server with more tools than fit in one answer says where to continue.
            match page.get("nextCursor").and_then(|c| c.as_str()) {
                Some(c) if !c.is_empty() => cursor = Some(c.to_string()),
                _ => break,
            }
        }
        Ok(out)
    }

    /// Run one of its tools.
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<(String, bool)> {
        let reply = self
            .request(
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
            )
            .await?;

        // The result is a list of content blocks. Only text is rendered; anything else is
        // named rather than dropped silently, so the model is not left wondering what it
        // was given.
        let mut text = String::new();
        if let Some(blocks) = reply.get("content").and_then(|c| c.as_array()) {
            for block in blocks {
                match block.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(s) = block.get("text").and_then(|t| t.as_str()) {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(s);
                        }
                    }
                    Some(other) => {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(&format!("[{other} content, not shown]"));
                    }
                    None => {}
                }
            }
        }
        let is_error = reply
            .get("isError")
            .and_then(|e| e.as_bool())
            .unwrap_or(false);
        Ok((text, is_error))
    }

    pub async fn shutdown(&self) {
        // MCP has no shutdown request; closing stdin is how a stdio server is told to go.
        let mut stdin = self.stdin.lock().await;
        let _ = stdin.shutdown().await;
    }

    async fn stderr_tail(&self) -> String {
        let buf = self.stderr.lock().await;
        if buf.trim().is_empty() {
            String::new()
        } else {
            format!("\nserver said:\n{}", buf.trim())
        }
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        self.write(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;

        let reply = tokio::time::timeout(TIMEOUT, rx)
            .await
            .map_err(|_| anyhow!("{method} timed out after {}s", TIMEOUT.as_secs()))?
            .map_err(|_| anyhow!("{method}: the server went away"))?;

        if let Some(err) = reply.get("error") {
            return Err(anyhow!("{method}: {err}"));
        }
        Ok(reply.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.write(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    async fn write(&self, payload: &Value) -> Result<()> {
        let mut line = serde_json::to_vec(payload)?;
        line.push(b'\n');
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(&line).await.context("writing to server")?;
        stdin.flush().await?;
        Ok(())
    }
}

async fn reader_loop(client: Arc<McpClient>, stdout: tokio::process::ChildStdout) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            tracing::warn!(server = %client.server, "unreadable line from server");
            continue;
        };
        // Only replies are awaited; a notification from the server has nobody waiting for
        // it and is recorded rather than acted on.
        match msg.get("id").and_then(|i| i.as_i64()) {
            Some(id) => {
                if let Some(tx) = client.pending.lock().await.remove(&id) {
                    let _ = tx.send(msg);
                }
            }
            None => {
                tracing::debug!(server = %client.server, method = ?msg.get("method"), "notification")
            }
        }
    }
}

async fn drain_stderr(stderr: tokio::process::ChildStderr, buf: Arc<Mutex<String>>) {
    use tokio::io::AsyncReadExt;
    let mut reader = BufReader::new(stderr);
    let mut chunk = [0u8; 1024];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                let mut held = buf.lock().await;
                held.push_str(&String::from_utf8_lossy(&chunk[..n]));
                if held.len() > STDERR_TAIL {
                    let cut = held.len() - STDERR_TAIL;
                    let cut = (cut..held.len())
                        .find(|&i| held.is_char_boundary(i))
                        .unwrap_or(held.len());
                    *held = held[cut..].to_string();
                }
            }
        }
    }
}
