//! A Model Context Protocol client, over a server's standard input and output, or over
//! HTTP.
//!
//! JSON-RPC again, but not framed the way the language server protocol frames it: one
//! message per line, no headers. Close enough to be confusing and different enough to
//! break if you assume. Over HTTP, each message is posted, and the answer comes back as
//! one JSON body or as a stream of events that may carry the server's own requests
//! before it.
//!
//! The server may ask things of the client as well as answer it. Its requests are told
//! apart from its replies by their method, and answered: `ping`, `roots/list` with the
//! project, and through the session's host a question for the model or a form for the
//! person. A server that stops fails every request still waiting at once.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{oneshot, Mutex};
use tokio_util::sync::CancellationToken;

/// The revision this client asks for.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// The revisions this client can speak, should the server answer with another.
const SPOKEN: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// How long to wait for any one request, when the configuration does not say.
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// How long a server told to stop has before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(2);

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
    /// What the server says of the tool: `readOnlyHint` among others.
    #[serde(default)]
    pub annotations: Value,
}

/// What the client and the task reading the server's output share.
struct Shared {
    server: String,
    /// The server's input, for a server the client started.
    stdin: Option<Mutex<ChildStdin>>,
    pending: Mutex<HashMap<i64, oneshot::Sender<Value>>>,
    stderr: Mutex<String>,
    alive: AtomicBool,
    /// Set when the server says its tools changed.
    tools_changed: AtomicBool,
    root: std::path::PathBuf,
    /// What answers the server's questions for the model and the person.
    host: super::host::SharedHost,
}

/// A server reached over HTTP.
struct Http {
    client: reqwest::Client,
    url: String,
    headers: Vec<(String, String)>,
    /// The session the server gave at the handshake, sent back with every message.
    session: Mutex<Option<String>>,
    /// Whether the token kept from signing in is sent, the configuration naming none.
    signs_in: bool,
}

/// One of a server's prompts, with what it takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptInfo {
    pub name: String,
    pub description: String,
    /// Each argument: its name, and whether it must be given.
    pub arguments: Vec<(String, bool)>,
}

pub struct McpClient {
    /// Named by the configuration, and used to prefix its tools.
    pub server: String,
    /// How it is reached: the command it was started with, or its url.
    pub origin: String,
    /// How many of its tools the session adopted.
    adopted: std::sync::atomic::AtomicUsize,
    /// The configuration it was started from, to start it again.
    pub started_from: std::sync::OnceLock<(crate::config::McpServer, std::path::PathBuf)>,
    child: Mutex<Option<Child>>,
    http: Option<Http>,
    shared: Arc<Shared>,
    next_id: AtomicI64,
    timeout: Duration,
    /// The revision the server answered with.
    pub version: String,
}

impl McpClient {
    /// Start a server and complete the handshake. Each request waits `timeout` at most.
    pub async fn start(
        server: &str,
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
        cwd: &std::path::Path,
        timeout: Duration,
        host: super::host::SharedHost,
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
        let shared = Arc::new(Shared {
            server: server.to_string(),
            stdin: Some(Mutex::new(stdin)),
            pending: Mutex::new(HashMap::new()),
            stderr: Mutex::new(String::new()),
            alive: AtomicBool::new(true),
            tools_changed: AtomicBool::new(false),
            root: cwd.to_path_buf(),
            host,
        });
        if let Some(stderr) = child.stderr.take() {
            let shared = shared.clone();
            tokio::spawn(async move { drain_stderr(stderr, shared).await });
        }
        {
            let shared = shared.clone();
            tokio::spawn(async move { reader_loop(shared, stdout).await });
        }

        Self {
            server: server.to_string(),
            origin: std::iter::once(command)
                .chain(args.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" "),
            adopted: Default::default(),
            started_from: Default::default(),
            child: Mutex::new(Some(child)),
            http: None,
            shared,
            next_id: AtomicI64::new(1),
            timeout,
            version: String::new(),
        }
        .handshake()
        .await
    }

    /// Reach a server over HTTP at `url`, sending `headers` with every message, and
    /// complete the handshake.
    pub async fn connect(
        server: &str,
        url: &str,
        headers: Vec<(String, String)>,
        root: &std::path::Path,
        timeout: Duration,
        host: super::host::SharedHost,
    ) -> Result<Arc<Self>> {
        let signs_in = !headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("authorization"));
        let shared = Arc::new(Shared {
            server: server.to_string(),
            stdin: None,
            pending: Mutex::new(HashMap::new()),
            stderr: Mutex::new(String::new()),
            alive: AtomicBool::new(true),
            tools_changed: AtomicBool::new(false),
            root: root.to_path_buf(),
            host,
        });
        Self {
            server: server.to_string(),
            origin: url.to_string(),
            adopted: Default::default(),
            started_from: Default::default(),
            child: Mutex::new(None),
            http: Some(Http {
                client: reqwest::Client::builder().build()?,
                url: url.to_string(),
                headers,
                session: Mutex::new(None),
                signs_in,
            }),
            shared,
            next_id: AtomicI64::new(1),
            timeout,
            version: String::new(),
        }
        .handshake()
        .await
    }

    /// Agree on the protocol with the server, or say why not.
    async fn handshake(mut self) -> Result<Arc<Self>> {
        let server = self.server.clone();
        let client = &mut self;
        let init = client
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {
                        "roots": { "listChanged": false },
                        "sampling": {},
                        "elicitation": {},
                    },
                    "clientInfo": { "name": "lokode", "version": env!("CARGO_PKG_VERSION") },
                }),
                None,
            )
            .await;
        let init = match init {
            Ok(v) => v,
            Err(e) => {
                let tail = client.stderr_tail().await;
                client.shutdown().await;
                return Err(anyhow!("{server} did not initialise: {e:#}{tail}"));
            }
        };
        let version = init
            .get("protocolVersion")
            .and_then(|v| v.as_str())
            .unwrap_or(PROTOCOL_VERSION)
            .to_string();
        if !SPOKEN.contains(&version.as_str()) {
            client.shutdown().await;
            return Err(anyhow!(
                "{server} speaks the protocol of {version}, which lokode does not ({})",
                SPOKEN.join(", ")
            ));
        }
        client.version = version;
        client
            .notify("notifications/initialized", json!({}))
            .await?;
        Ok(Arc::new(self))
    }

    /// How many of its tools the session adopted.
    pub fn adopted(&self) -> usize {
        self.adopted.load(Ordering::Relaxed)
    }

    pub fn set_adopted(&self, n: usize) {
        self.adopted.store(n, Ordering::Relaxed);
    }

    /// Its prompts, with the arguments each takes. Empty when it offers none.
    pub async fn prompts(&self) -> Vec<PromptInfo> {
        let Ok(page) = self.request("prompts/list", json!({}), None).await else {
            return Vec::new();
        };
        page.get("prompts")
            .and_then(|p| p.as_array())
            .into_iter()
            .flatten()
            .filter_map(|p| {
                Some(PromptInfo {
                    name: p.get("name")?.as_str()?.to_string(),
                    description: p
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    arguments: p
                        .get("arguments")
                        .and_then(|a| a.as_array())
                        .into_iter()
                        .flatten()
                        .filter_map(|a| {
                            Some((
                                a.get("name")?.as_str()?.to_string(),
                                a.get("required").and_then(|r| r.as_bool()).unwrap_or(false),
                            ))
                        })
                        .collect(),
                })
            })
            .collect()
    }

    /// What answers the server's questions, shared with a server started in its place.
    pub fn host(&self) -> super::host::SharedHost {
        self.shared.host.clone()
    }

    /// Whether the server is still running.
    pub fn alive(&self) -> bool {
        self.shared.alive.load(Ordering::Relaxed)
    }

    /// Whether the server said its tools changed since this was last asked.
    pub fn take_tools_changed(&self) -> bool {
        self.shared.tools_changed.swap(false, Ordering::Relaxed)
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
            let page = self.request("tools/list", params, None).await?;
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

    /// What this server offers besides tools: its resources and its prompts.
    ///
    /// A server that does not do either answers with an error, which is not a failure of
    /// the server: the protocol makes both optional. Both come back empty then.
    pub async fn list_resources(&self) -> Vec<(String, String)> {
        self.list_named("resources/list", "resources", "uri").await
    }

    pub async fn list_prompts(&self) -> Vec<(String, String)> {
        self.list_named("prompts/list", "prompts", "name").await
    }

    /// The name and description of each row of a listing, whatever it is called there.
    async fn list_named(&self, method: &str, field: &str, key: &str) -> Vec<(String, String)> {
        let Ok(page) = self.request(method, json!({}), None).await else {
            return Vec::new();
        };
        page.get(field)
            .and_then(|r| r.as_array())
            .map(|rows| {
                rows.iter()
                    .filter_map(|r| {
                        let name = r.get(key).or_else(|| r.get("name"))?.as_str()?;
                        let about = r
                            .get("description")
                            .and_then(|d| d.as_str())
                            .unwrap_or_default();
                        Some((name.to_string(), about.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Read one resource, as text.
    pub async fn read_resource(&self, uri: &str) -> Result<String> {
        let reply = self
            .request("resources/read", json!({ "uri": uri }), None)
            .await?;
        let text = reply
            .get("contents")
            .and_then(|c| c.as_array())
            .map(|rows| {
                rows.iter()
                    .filter_map(|r| r.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        if text.is_empty() {
            anyhow::bail!("{uri} came back with nothing this client can read as text");
        }
        Ok(text)
    }

    /// Take one of the server's prompts, rendered with the arguments given.
    pub async fn get_prompt(&self, name: &str, arguments: Value) -> Result<String> {
        let reply = self
            .request(
                "prompts/get",
                json!({ "name": name, "arguments": arguments }),
                None,
            )
            .await?;
        let text = reply
            .get("messages")
            .and_then(|m| m.as_array())
            .map(|rows| {
                rows.iter()
                    .filter_map(|m| {
                        m.get("content")
                            .and_then(|c| c.get("text").or(Some(c)))
                            .and_then(|t| t.as_str())
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        if text.is_empty() {
            anyhow::bail!("the prompt {name} came back with nothing this client can read");
        }
        Ok(text)
    }

    /// Run one of its tools. Stopped with `cancel`, the server is told.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        cancel: &CancellationToken,
    ) -> Result<(String, bool)> {
        let reply = self
            .request(
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
                Some(cancel),
            )
            .await?;
        let is_error = reply
            .get("isError")
            .and_then(|e| e.as_bool())
            .unwrap_or(false);
        Ok((render_result(&reply), is_error))
    }

    /// Tell the server to stop: close its input, and kill it if it is still there after a
    /// moment.
    pub async fn shutdown(&self) {
        // A session over HTTP is ended by deleting it.
        if let Some(http) = &self.http {
            if let Some(session) = http.session.lock().await.clone() {
                let mut req = http
                    .client
                    .delete(&http.url)
                    .header("Mcp-Session-Id", session);
                for (k, v) in &http.headers {
                    req = req.header(k, v);
                }
                let _ = tokio::time::timeout(STOP_GRACE, req.send()).await;
            }
            return;
        }
        // MCP has no shutdown request; closing stdin is how a stdio server is told to go.
        if let Some(stdin) = &self.shared.stdin {
            let _ = stdin.lock().await.shutdown().await;
        }
        if let Some(mut child) = self.child.lock().await.take() {
            if tokio::time::timeout(STOP_GRACE, child.wait())
                .await
                .is_err()
            {
                let _ = child.kill().await;
            }
        }
    }

    /// What the server last wrote to its error output, for a message about it.
    pub async fn stderr_tail(&self) -> String {
        let buf = self.shared.stderr.lock().await;
        if buf.trim().is_empty() {
            String::new()
        } else {
            format!("\nserver said:\n{}", buf.trim())
        }
    }

    /// Send a request and wait for its answer. When `cancel` fires first, the server is
    /// told the request is withdrawn.
    async fn request(
        &self,
        method: &str,
        params: Value,
        cancel: Option<&CancellationToken>,
    ) -> Result<Value> {
        if !self.alive() {
            return Err(anyhow!("{method}: the server is not running"));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let payload = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let never = CancellationToken::new();
        let cancel = cancel.unwrap_or(&never);
        if let Some(http) = &self.http {
            let reply = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    let _ = self
                        .notify(
                            "notifications/cancelled",
                            json!({"requestId": id, "reason": "the user stopped it"}),
                        )
                        .await;
                    return Err(anyhow!("{method}: stopped"));
                }
                r = tokio::time::timeout(self.timeout, self.post(http, &payload, Some(id))) => match r {
                    Err(_) => return Err(anyhow!("{method} timed out after {}s", self.timeout.as_secs())),
                    Ok(r) => r.with_context(|| method.to_string())?,
                },
            };
            if let Some(err) = reply.get("error") {
                return Err(anyhow!("{method}: {err}"));
            }
            return Ok(reply.get("result").cloned().unwrap_or(Value::Null));
        }
        let (tx, rx) = oneshot::channel();
        self.shared.pending.lock().await.insert(id, tx);

        if let Err(e) = self.write(&payload).await {
            self.shared.pending.lock().await.remove(&id);
            return Err(e);
        }

        let reply = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                self.shared.pending.lock().await.remove(&id);
                let _ = self
                    .notify(
                        "notifications/cancelled",
                        json!({"requestId": id, "reason": "the user stopped it"}),
                    )
                    .await;
                return Err(anyhow!("{method}: stopped"));
            }
            r = tokio::time::timeout(self.timeout, rx) => match r {
                Err(_) => {
                    self.shared.pending.lock().await.remove(&id);
                    return Err(anyhow!("{method} timed out after {}s", self.timeout.as_secs()));
                }
                Ok(Err(_)) => {
                    return Err(anyhow!(
                        "{method}: the server stopped{}",
                        self.stderr_tail().await
                    ))
                }
                Ok(Ok(reply)) => reply,
            },
        };

        if let Some(err) = reply.get("error") {
            return Err(anyhow!("{method}: {err}"));
        }
        Ok(reply.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let payload = json!({"jsonrpc": "2.0", "method": method, "params": params});
        match &self.http {
            Some(http) => self.post(http, &payload, None).await.map(|_| ()),
            None => self.write(&payload).await,
        }
    }

    async fn write(&self, payload: &Value) -> Result<()> {
        write_to(&self.shared, payload).await
    }

    /// Post one message over HTTP. For a request, `id` names it, and its reply is read
    /// from the body: one JSON message, or a stream of events where the server's own
    /// requests are answered as they come. For anything else, nothing is read.
    async fn post(&self, http: &Http, payload: &Value, id: Option<i64>) -> Result<Value> {
        use futures::StreamExt;
        let mut req = http
            .client
            .post(&http.url)
            .header("Accept", "application/json, text/event-stream")
            .json(payload);
        for (k, v) in &http.headers {
            req = req.header(k, v);
        }
        if let Some(session) = http.session.lock().await.clone() {
            req = req.header("Mcp-Session-Id", session);
        }
        if http.signs_in {
            match super::oauth::bearer(&http.client, &http.url).await {
                Ok(Some(token)) => req = req.bearer_auth(token),
                Ok(None) => {}
                Err(e) => anyhow::bail!("{e:#}: sign in again with /mcp login {}", self.server),
            }
        }
        if !self.version.is_empty() {
            req = req.header("MCP-Protocol-Version", &self.version);
        }
        let resp = req.send().await.context("reaching the server")?;
        if let Some(session) = resp
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|v| v.to_str().ok())
        {
            *http.session.lock().await = Some(session.to_string());
        }
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED && http.signs_in {
            anyhow::bail!(
                "the server wants a signed-in user: /mcp login {}",
                self.server
            );
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("the server answered {status}: {}", body.trim());
        }
        let Some(id) = id else {
            return Ok(Value::Null);
        };
        let is_stream = resp
            .headers()
            .get("Content-Type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|t| t.starts_with("text/event-stream"));
        if !is_stream {
            return Ok(resp.json().await?);
        }
        let mut events = eventsource_stream::Eventsource::eventsource(resp.bytes_stream());
        while let Some(event) = events.next().await {
            let event = event.map_err(|e| anyhow!("reading the server's events: {e}"))?;
            let Ok(msg) = serde_json::from_str::<Value>(&event.data) else {
                continue;
            };
            let method = msg.get("method").and_then(|m| m.as_str());
            match (msg.get("id"), method) {
                (Some(their), Some(method)) => {
                    let params = msg.get("params").cloned().unwrap_or(Value::Null);
                    let reply = reply_to(&self.shared, their, method, &params).await;
                    // Posted beside the stream, which is still open for the reply.
                    let _ = Box::pin(self.post(http, &reply, None)).await;
                }
                (Some(theirs), None) if theirs.as_i64() == Some(id) => return Ok(msg),
                (None, Some("notifications/tools/list_changed")) => {
                    self.shared.tools_changed.store(true, Ordering::Relaxed);
                }
                _ => {}
            }
        }
        anyhow::bail!("the server's events ended without the reply")
    }
}

async fn write_to(shared: &Shared, payload: &Value) -> Result<()> {
    let Some(stdin) = &shared.stdin else {
        anyhow::bail!("this server has no input to write to");
    };
    let mut line = serde_json::to_vec(payload)?;
    line.push(b'\n');
    let mut stdin = stdin.lock().await;
    stdin.write_all(&line).await.context("writing to server")?;
    stdin.flush().await?;
    Ok(())
}

/// The client's reply to a request of the server's, with the id it gave.
async fn reply_to(shared: &Shared, id: &Value, method: &str, params: &Value) -> Value {
    match answer(shared, method, params).await {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, message)) => json!({
            "jsonrpc": "2.0", "id": id,
            "error": {"code": code, "message": message}
        }),
    }
}

/// What the client answers a request from the server.
async fn answer(shared: &Shared, method: &str, params: &Value) -> Result<Value, (i64, String)> {
    let host = || {
        shared
            .host
            .get()
            .ok_or((-32603, "the session is not ready yet".to_string()))
    };
    match method {
        "sampling/createMessage" => host()?.sample(&shared.server, params).await,
        "elicitation/create" => host()?.elicit(&shared.server, params).await,
        "ping" => Ok(json!({})),
        "roots/list" => {
            let uri = url::Url::from_directory_path(&shared.root)
                .map(|u| u.to_string())
                .unwrap_or_default();
            let name = shared
                .root
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            Ok(json!({"roots": [{"uri": uri, "name": name}]}))
        }
        other => Err((-32601, format!("lokode does not answer {other}"))),
    }
}

async fn reader_loop(shared: Arc<Shared>, stdout: tokio::process::ChildStdout) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            tracing::warn!(server = %shared.server, "unreadable line from server");
            continue;
        };
        let method = msg.get("method").and_then(|m| m.as_str());
        match (msg.get("id"), method) {
            // A request of the server's own, answered with the id it gave. Answered beside
            // the reading, since the person or the model may take a while.
            (Some(id), Some(method)) => {
                let (shared, id, method) = (shared.clone(), id.clone(), method.to_string());
                let params = msg.get("params").cloned().unwrap_or(Value::Null);
                tokio::spawn(async move {
                    let reply = reply_to(&shared, &id, &method, &params).await;
                    let _ = write_to(&shared, &reply).await;
                });
            }
            (Some(id), None) => {
                if let Some(id) = id.as_i64() {
                    if let Some(tx) = shared.pending.lock().await.remove(&id) {
                        let _ = tx.send(msg);
                    }
                }
            }
            (None, Some("notifications/tools/list_changed")) => {
                shared.tools_changed.store(true, Ordering::Relaxed);
            }
            (None, method) => {
                tracing::debug!(server = %shared.server, ?method, "notification")
            }
        }
    }
    // Every request still waiting is answered now, with the server gone.
    shared.alive.store(false, Ordering::Relaxed);
    shared.pending.lock().await.clear();
    tracing::warn!(server = %shared.server, "MCP server stopped");
}

async fn drain_stderr(stderr: tokio::process::ChildStderr, shared: Arc<Shared>) {
    use tokio::io::AsyncReadExt;
    let mut reader = BufReader::new(stderr);
    let mut chunk = [0u8; 1024];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                let mut held = shared.stderr.lock().await;
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

/// A tool's result as text: its text blocks, the text of the resources it embeds, a line
/// naming each block that is not text, and its structured content when it has nothing
/// else to say.
fn render_result(reply: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    for block in reply
        .get("content")
        .and_then(|c| c.as_array())
        .into_iter()
        .flatten()
    {
        let text = |v: Option<&Value>| v.and_then(|t| t.as_str()).map(str::to_string);
        match block.get("type").and_then(|t| t.as_str()) {
            Some("text") => parts.extend(text(block.get("text"))),
            Some("resource") => match text(block.pointer("/resource/text")) {
                Some(t) => parts.push(t),
                None => parts.push(format!(
                    "[a resource, {}, not shown]",
                    text(block.pointer("/resource/uri")).unwrap_or_default()
                )),
            },
            Some("resource_link") => parts.push(format!(
                "[a link to {}]",
                text(block.get("uri")).unwrap_or_default()
            )),
            Some(other) => parts.push(format!(
                "[{other} content{}, not shown]",
                text(block.get("mimeType"))
                    .map(|m| format!(", {m}"))
                    .unwrap_or_default()
            )),
            None => {}
        }
    }
    if parts.is_empty() {
        if let Some(structured) = reply.get("structuredContent") {
            parts.push(serde_json::to_string_pretty(structured).unwrap_or_default());
        }
    }
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Text and embedded text come through, other blocks are named, and structured content
    /// speaks when nothing else does.
    #[test]
    fn a_result_is_rendered_as_text() {
        let reply = json!({"content": [
            {"type": "text", "text": "one"},
            {"type": "image", "data": "...", "mimeType": "image/png"},
            {"type": "resource", "resource": {"uri": "file:///a", "text": "two"}},
            {"type": "resource_link", "uri": "file:///b"}
        ]});
        assert_eq!(
            render_result(&reply),
            "one\n[image content, image/png, not shown]\ntwo\n[a link to file:///b]"
        );
        let structured = json!({"content": [], "structuredContent": {"n": 1}});
        assert!(render_result(&structured).contains("\"n\": 1"));
    }

    /// A small server written in shell. Before it answers the handshake it pings the
    /// client with the handshake's own id and asks for its roots. It answers one tool
    /// call, then exits on the second.
    const SERVER: &str = r#"
read -r init
echo '{"jsonrpc":"2.0","id":1,"method":"ping"}'
read -r pong
echo '{"jsonrpc":"2.0","id":7,"method":"roots/list"}'
read -r roots
echo "$roots" > roots.json
echo '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}}}}'
read -r initialized
read -r call
echo '{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"called"}]}}'
read -r second
exit 3
"#;

    /// A request of the server's own with the same id as one of ours is answered and not
    /// taken for our reply, and a server that stops fails what waits on it at once.
    #[tokio::test]
    async fn the_server_is_answered_and_its_end_is_noticed() {
        let dir = tempfile::tempdir().unwrap();
        let client = McpClient::start(
            "t",
            "sh",
            &["-c".to_string(), SERVER.to_string()],
            &HashMap::new(),
            dir.path(),
            Duration::from_secs(10),
            Default::default(),
        )
        .await
        .unwrap();
        assert_eq!(client.version, "2025-06-18");
        let never = CancellationToken::new();
        let (text, is_error) = client.call_tool("x", json!({}), &never).await.unwrap();
        assert_eq!(text, "called");
        assert!(!is_error);
        let roots = std::fs::read_to_string(dir.path().join("roots.json")).unwrap();
        assert!(
            roots.contains("\"id\":7") && roots.contains("file://"),
            "{roots}"
        );

        let started = std::time::Instant::now();
        let err = client.call_tool("x", json!({}), &never).await.unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "waited for the timeout"
        );
        assert!(format!("{err}").contains("stopped"), "{err}");
        assert!(!client.alive());
    }

    /// One HTTP exchange of the test server: the request's headers and body, and what to
    /// answer it with.
    async fn exchange(
        stream: &mut tokio::net::TcpStream,
        answer: impl FnOnce(&str, &Value) -> String,
    ) -> (String, Value) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let (head, body) = loop {
            let n = stream.read(&mut chunk).await.unwrap();
            buf.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&buf).to_string();
            if let Some((head, body)) = text.split_once("\r\n\r\n") {
                let length: usize = head
                    .lines()
                    .find_map(|l| {
                        l.to_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().to_string())
                    })
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                if body.len() >= length {
                    break (head.to_string(), body[..length].to_string());
                }
            }
        };
        let msg: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let reply = answer(&head, &msg);
        stream.write_all(reply.as_bytes()).await.unwrap();
        stream.shutdown().await.ok();
        (head, msg)
    }

    /// Over HTTP the session the server gives is sent back, a reply may come as a stream
    /// of events, and a request of the server's inside that stream is answered.
    #[tokio::test]
    async fn a_server_over_http_is_spoken_to() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
        let kept = seen.clone();
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let got = exchange(&mut stream, |_, msg| {
                    match msg.get("method").and_then(|m| m.as_str()) {
                        Some("initialize") => {
                            let body = json!({"jsonrpc": "2.0", "id": msg["id"], "result": {"protocolVersion": "2025-06-18"}}).to_string();
                            format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: s-42\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
                        }
                        Some("tools/call") => {
                            let ping = json!({"jsonrpc": "2.0", "id": 99, "method": "ping"});
                            let done = json!({"jsonrpc": "2.0", "id": msg["id"], "result": {"content": [{"type": "text", "text": "over http"}]}});
                            let body = format!("data: {ping}\n\ndata: {done}\n\n");
                            format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
                        }
                        _ => "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
                    }
                })
                .await;
                kept.lock().await.push(got);
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let client = McpClient::connect(
            "h",
            &url,
            Vec::new(),
            dir.path(),
            Duration::from_secs(10),
            Default::default(),
        )
        .await
        .unwrap();
        let (text, _) = client
            .call_tool("x", json!({}), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(text, "over http");
        let seen = seen.lock().await;
        let call = seen
            .iter()
            .find(|(_, m)| m["method"] == "tools/call")
            .expect("the call never arrived");
        assert!(
            call.0.to_lowercase().contains("mcp-session-id: s-42"),
            "{}",
            call.0
        );
        assert!(
            seen.iter()
                .any(|(_, m)| m["id"] == 99 && m.get("result").is_some()),
            "the ping was not answered: {seen:?}"
        );
    }
}
