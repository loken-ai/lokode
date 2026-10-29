//! One language server process, spoken to over stdio.
//!
//! Requests carry an id the server answers with. The server sends requests of its own,
//! which are answered here, and notifications: the diagnostics it publishes and the
//! progress of its work, both kept for the tools to read.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use lsp_types::Url;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{oneshot, Mutex, Notify};

use crate::lsp::language::ServerSpec;
use crate::lsp::position::Encoding;

/// Bytes of the server's own complaints kept for when something goes wrong.
const STDERR_TAIL: usize = 4 * 1024;

/// How long one request may wait for its answer.
const REQUEST_WAIT: Duration = Duration::from_secs(30);

/// How long a server is given to agree to stop.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(2);

/// The protocol's code for a method the receiver does not handle.
const METHOD_NOT_FOUND: i64 = -32601;

/// What was last sent to the server for one file.
pub(crate) struct OpenDoc {
    pub version: i32,
    pub text: String,
}

/// What the server last published for one file.
#[derive(Debug, Clone)]
pub struct Published {
    /// The version of the file it describes, when the server says.
    pub version: Option<i32>,
    pub diagnostics: Vec<lsp_types::Diagnostic>,
    pub at: Instant,
}

/// Work the server reports while it does it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    pub title: String,
    pub message: Option<String>,
    pub percentage: Option<u32>,
}

/// What the server said it can do, read from its answer to `initialize`.
#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    pub encoding: Encoding,
    pub hover: bool,
    pub definition: bool,
    pub references: bool,
    pub document_symbol: bool,
    pub workspace_symbol: bool,
    pub implementation: bool,
    pub call_hierarchy: bool,
    /// Whether it wants to hear that a file was saved.
    pub save: bool,
}

impl Capabilities {
    fn read(caps: &Value) -> Self {
        let has = |key: &str| {
            caps.get(key)
                .is_some_and(|v| !v.is_null() && v != &Value::Bool(false))
        };
        let sync = caps.get("textDocumentSync");
        let save = match sync.and_then(|s| s.get("save")) {
            Some(Value::Bool(b)) => *b,
            Some(Value::Object(_)) => true,
            _ => false,
        };
        Self {
            encoding: caps
                .get("positionEncoding")
                .and_then(Value::as_str)
                .map(Encoding::from_name)
                .unwrap_or_default(),
            hover: has("hoverProvider"),
            definition: has("definitionProvider"),
            references: has("referencesProvider"),
            document_symbol: has("documentSymbolProvider"),
            workspace_symbol: has("workspaceSymbolProvider"),
            implementation: has("implementationProvider"),
            call_hierarchy: has("callHierarchyProvider"),
            save,
        }
    }
}

/// What this client tells a server it can handle.
fn client_capabilities() -> Value {
    json!({
        "general": {"positionEncodings": ["utf-8", "utf-16"]},
        "window": {"workDoneProgress": true},
        "workspace": {
            "configuration": true,
            "workspaceFolders": true,
            "didChangeWatchedFiles": {"dynamicRegistration": true},
            "symbol": {},
            "applyEdit": false
        },
        "textDocument": {
            "synchronization": {"didSave": true},
            "publishDiagnostics": {
                "relatedInformation": true,
                "versionSupport": true,
                "codeDescriptionSupport": true
            },
            "hover": {"contentFormat": ["markdown", "plaintext"]},
            "definition": {"linkSupport": false},
            "implementation": {"linkSupport": false},
            "references": {},
            "documentSymbol": {"hierarchicalDocumentSymbolSupport": true},
            "callHierarchy": {}
        },
        "experimental": {"serverStatusNotification": true}
    })
}

pub struct Server {
    pub spec: ServerSpec,
    root: PathBuf,
    _child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    next_id: AtomicI64,
    pending: Mutex<HashMap<i64, oneshot::Sender<Value>>>,
    published: Mutex<HashMap<Url, Published>>,
    pub(crate) opened: Mutex<HashMap<PathBuf, OpenDoc>>,
    stderr: Arc<Mutex<String>>,
    progress: Mutex<HashMap<String, Progress>>,
    /// False while the server says it is still working through the project.
    quiescent: AtomicBool,
    caps: OnceLock<Capabilities>,
    alive: AtomicBool,
    /// Woken when something a waiter looks at changes: a publication, progress, the end.
    pub(crate) changed: Notify,
}

impl Server {
    /// Start the server for `spec` under `root` and agree on what each side can do.
    pub async fn spawn(spec: &ServerSpec, root: &Path) -> Result<Arc<Self>> {
        let mut child = tokio::process::Command::new(&spec.command)
            .args(&spec.args)
            .envs(spec.env.iter().map(|(k, v)| (k, v)))
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| anyhow!("{} could not be started: {e}", spec.command))?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        let stderr = Arc::new(Mutex::new(String::new()));
        if let Some(err) = child.stderr.take() {
            let buf = stderr.clone();
            tokio::spawn(async move { drain_stderr(err, buf).await });
        }
        let server = Arc::new(Self {
            spec: spec.clone(),
            root: root.to_path_buf(),
            _child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            next_id: AtomicI64::new(1),
            pending: Mutex::new(HashMap::new()),
            published: Mutex::new(HashMap::new()),
            opened: Mutex::new(HashMap::new()),
            stderr,
            progress: Mutex::new(HashMap::new()),
            quiescent: AtomicBool::new(true),
            caps: OnceLock::new(),
            alive: AtomicBool::new(true),
            changed: Notify::new(),
        });
        {
            let server = server.clone();
            tokio::spawn(async move { server.read(stdout).await });
        }

        let root_uri = Url::from_directory_path(root).map_err(|_| anyhow!("bad root url"))?;
        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "root".into());
        let init = json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "workspaceFolders": [{"uri": root_uri, "name": name}],
            "capabilities": client_capabilities(),
            "initializationOptions": spec.init,
            "clientInfo": {"name": "lokode", "version": env!("CARGO_PKG_VERSION")},
        });
        let answer = match server.request("initialize", init).await {
            Ok(a) => a,
            Err(e) => {
                return Err(anyhow!(
                    "{} did not initialise: {e}{}",
                    spec.command,
                    server.stderr_tail().await
                ))
            }
        };
        let caps = Capabilities::read(answer.get("capabilities").unwrap_or(&Value::Null));
        let _ = server.caps.set(caps);
        server.notify("initialized", json!({})).await?;
        if let Some(settings) = &spec.settings {
            server
                .notify(
                    "workspace/didChangeConfiguration",
                    json!({"settings": settings}),
                )
                .await?;
        }
        Ok(server)
    }

    /// The project the server was started on.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn caps(&self) -> Capabilities {
        self.caps.get().cloned().unwrap_or_default()
    }

    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    /// What the server has said on stderr, formatted for appending to an error.
    pub async fn stderr_tail(&self) -> String {
        let buf = self.stderr.lock().await;
        match buf.trim().is_empty() {
            true => String::new(),
            false => format!("\nserver said:\n{}", buf.trim()),
        }
    }

    /// The work the server reports doing now, oldest first.
    pub async fn progress(&self) -> Vec<Progress> {
        self.progress.lock().await.values().cloned().collect()
    }

    /// Whether the server has work in hand: reported progress, or a status saying it is
    /// not yet through the project.
    pub async fn busy(&self) -> bool {
        !self.quiescent.load(Ordering::Acquire) || !self.progress.lock().await.is_empty()
    }

    /// What the server last published for `uri`.
    pub async fn published(&self, uri: &Url) -> Option<Published> {
        self.published.lock().await.get(uri).cloned()
    }

    /// Everything the server has published, by file.
    pub async fn all_published(&self) -> HashMap<Url, Published> {
        self.published.lock().await.clone()
    }

    /// Ask the server to stop, and wait briefly for it to agree.
    pub async fn shutdown(&self) {
        if !self.alive() {
            return;
        }
        let _ = tokio::time::timeout(SHUTDOWN_WAIT, self.request("shutdown", Value::Null)).await;
        let _ = self.notify("exit", Value::Null).await;
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if !self.alive() {
            return Err(anyhow!("{} has stopped", self.spec.command));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let frame = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(e) = self.write(&frame).await {
            self.pending.lock().await.remove(&id);
            return Err(e);
        }
        let reply = match tokio::time::timeout(REQUEST_WAIT, rx).await {
            Ok(Ok(reply)) => reply,
            Ok(Err(_)) => {
                return Err(anyhow!(
                    "{} stopped before answering {method}",
                    self.spec.command
                ))
            }
            Err(_) => {
                self.pending.lock().await.remove(&id);
                return Err(anyhow!(
                    "{method} got no answer in {}s",
                    REQUEST_WAIT.as_secs()
                ));
            }
        };
        if let Some(err) = reply.get("error") {
            let message = err.get("message").and_then(Value::as_str).unwrap_or("");
            return Err(anyhow!("{method}: {message}"));
        }
        Ok(reply.get("result").cloned().unwrap_or(Value::Null))
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.write(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    async fn write(&self, payload: &Value) -> Result<()> {
        let body = serde_json::to_vec(payload)?;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(header.as_bytes()).await?;
        stdin.write_all(&body).await?;
        stdin.flush().await?;
        Ok(())
    }

    /// Make the server's copy of `path` match the file on disk. Returns its URI, the
    /// version sent, and whether anything was sent.
    pub async fn sync(&self, path: &Path) -> Result<(Url, i32, bool)> {
        let uri = Url::from_file_path(path).map_err(|_| anyhow!("bad file url"))?;
        let text = tokio::fs::read_to_string(path).await?;
        let language_id = self
            .spec
            .language_id(path)
            .unwrap_or("plaintext")
            .to_string();
        let mut opened = self.opened.lock().await;
        let (version, sent) = match opened.get_mut(path) {
            None => {
                self.notify(
                    "textDocument/didOpen",
                    json!({"textDocument": {
                        "uri": uri, "languageId": language_id, "version": 1, "text": text
                    }}),
                )
                .await?;
                opened.insert(path.to_path_buf(), OpenDoc { version: 1, text });
                (1, true)
            }
            Some(doc) if doc.text != text => {
                doc.version += 1;
                doc.text = text.clone();
                // The whole text: the tools rewrite files, and ranges here would invent a
                // precision the caller never had.
                self.notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": {"uri": uri, "version": doc.version},
                        "contentChanges": [{"text": text}]
                    }),
                )
                .await?;
                (doc.version, true)
            }
            Some(doc) => (doc.version, false),
        };
        Ok((uri, version, sent))
    }

    /// The text last sent for `path`, or the file on disk when none was.
    pub async fn text_of(&self, path: &Path) -> Option<String> {
        if let Some(doc) = self.opened.lock().await.get(path) {
            return Some(doc.text.clone());
        }
        std::fs::read_to_string(path).ok()
    }

    async fn read(self: Arc<Self>, stdout: ChildStdout) {
        let mut reader = BufReader::new(stdout);
        while let Some(msg) = read_message(&mut reader).await {
            let method = msg
                .get("method")
                .and_then(Value::as_str)
                .map(str::to_string);
            match (method, msg.get("id").cloned()) {
                (Some(method), Some(id)) => {
                    let params = msg.get("params").cloned().unwrap_or(Value::Null);
                    let answer = match self.answer(&method, &params) {
                        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                        Err(message) => json!({"jsonrpc": "2.0", "id": id, "error": {
                            "code": METHOD_NOT_FOUND, "message": message
                        }}),
                    };
                    let _ = self.write(&answer).await;
                }
                (None, Some(id)) => {
                    let waiter = match id.as_i64() {
                        Some(id) => self.pending.lock().await.remove(&id),
                        None => None,
                    };
                    if let Some(tx) = waiter {
                        let _ = tx.send(msg);
                    }
                }
                (Some(method), None) => {
                    self.heard(&method, msg.get("params").unwrap_or(&Value::Null))
                        .await
                }
                (None, None) => {}
            }
        }
        // The server is gone: every request waiting on it fails now rather than at its
        // timeout, and whoever waits on a change is told.
        self.alive.store(false, Ordering::Release);
        self.pending.lock().await.clear();
        self.progress.lock().await.clear();
        self.changed.notify_waiters();
    }

    /// The answer to a request the server sent.
    fn answer(&self, method: &str, params: &Value) -> std::result::Result<Value, String> {
        match method {
            "workspace/configuration" => {
                let items = params.get("items").and_then(Value::as_array);
                Ok(Value::Array(
                    items
                        .into_iter()
                        .flatten()
                        .map(|item| {
                            let section = item.get("section").and_then(Value::as_str);
                            setting(self.spec.settings.as_ref(), section)
                        })
                        .collect(),
                ))
            }
            "workspace/workspaceFolders" => {
                let uri = Url::from_directory_path(&self.root).map_err(|_| "bad root url")?;
                Ok(json!([{"uri": uri, "name": "root"}]))
            }
            "client/registerCapability"
            | "client/unregisterCapability"
            | "window/workDoneProgress/create"
            | "window/showMessageRequest"
            | "workspace/diagnostic/refresh"
            | "workspace/semanticTokens/refresh"
            | "workspace/inlayHint/refresh"
            | "workspace/codeLens/refresh" => Ok(Value::Null),
            "workspace/applyEdit" => Ok(json!({
                "applied": false,
                "failureReason": "edits are made through the agent's own tools"
            })),
            _ => Err(format!("{method} is not handled")),
        }
    }

    /// A notification the server sent.
    async fn heard(&self, method: &str, params: &Value) {
        match method {
            "textDocument/publishDiagnostics" => {
                let Some(uri) = params
                    .get("uri")
                    .and_then(Value::as_str)
                    .and_then(|u| Url::parse(u).ok())
                else {
                    return;
                };
                let diagnostics = params
                    .get("diagnostics")
                    .cloned()
                    .and_then(|d| serde_json::from_value(d).ok())
                    .unwrap_or_default();
                let version = params
                    .get("version")
                    .and_then(Value::as_i64)
                    .map(|v| v as i32);
                self.published.lock().await.insert(
                    uri,
                    Published {
                        version,
                        diagnostics,
                        at: Instant::now(),
                    },
                );
            }
            "$/progress" => {
                let token = match params.get("token") {
                    Some(Value::String(s)) => s.clone(),
                    Some(other) => other.to_string(),
                    None => return,
                };
                let value = params.get("value").unwrap_or(&Value::Null);
                let text = |k: &str| value.get(k).and_then(Value::as_str).map(str::to_string);
                let percentage = value
                    .get("percentage")
                    .and_then(Value::as_u64)
                    .map(|p| p as u32);
                let mut progress = self.progress.lock().await;
                match value.get("kind").and_then(Value::as_str) {
                    Some("begin") => {
                        progress.insert(
                            token,
                            Progress {
                                title: text("title").unwrap_or_default(),
                                message: text("message"),
                                percentage,
                            },
                        );
                    }
                    Some("report") => {
                        if let Some(p) = progress.get_mut(&token) {
                            if let Some(m) = text("message") {
                                p.message = Some(m);
                            }
                            if percentage.is_some() {
                                p.percentage = percentage;
                            }
                        }
                    }
                    Some("end") => {
                        progress.remove(&token);
                    }
                    _ => return,
                }
            }
            "experimental/serverStatus" => {
                let quiescent = params
                    .get("quiescent")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                self.quiescent.store(quiescent, Ordering::Release);
            }
            "window/showMessage" | "window/logMessage" => {
                // Errors only: they are what explains a server that stopped answering.
                if params.get("type").and_then(Value::as_i64) == Some(1) {
                    if let Some(m) = params.get("message").and_then(Value::as_str) {
                        keep_tail(&self.stderr, m).await;
                    }
                }
                return;
            }
            _ => return,
        }
        self.changed.notify_waiters();
    }
}

/// The value of `section` in `settings`, a dotted path, or null where there is none.
fn setting(settings: Option<&Value>, section: Option<&str>) -> Value {
    let Some(mut at) = settings else {
        return Value::Null;
    };
    if let Some(section) = section {
        for key in section.split('.') {
            match at.get(key) {
                Some(v) => at = v,
                None => return Value::Null,
            }
        }
    }
    at.clone()
}

/// One message from the server, or nothing once its output has ended.
async fn read_message(reader: &mut BufReader<ChildStdout>) -> Option<Value> {
    loop {
        let mut length: Option<usize> = None;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).await.ok()? == 0 {
                return None;
            }
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some(rest) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                length = rest.trim().parse().ok();
            }
        }
        let Some(length) = length else {
            continue;
        };
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body).await.ok()?;
        if let Ok(msg) = serde_json::from_slice(&body) {
            return Some(msg);
        }
    }
}

/// Keep the tail of a language server's stderr.
async fn drain_stderr(stderr: tokio::process::ChildStderr, buf: Arc<Mutex<String>>) {
    let mut reader = BufReader::new(stderr);
    let mut chunk = [0u8; 1024];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => keep_tail(&buf, &String::from_utf8_lossy(&chunk[..n])).await,
        }
    }
}

/// Add `text` to `buf`, keeping only its last [`STDERR_TAIL`] bytes.
async fn keep_tail(buf: &Mutex<String>, text: &str) {
    let mut held = buf.lock().await;
    held.push_str(text);
    if !text.ends_with('\n') {
        held.push('\n');
    }
    if held.len() > STDERR_TAIL {
        let cut = held.len() - STDERR_TAIL;
        let cut = (cut..held.len())
            .find(|&i| held.is_char_boundary(i))
            .unwrap_or(held.len());
        *held = held[cut..].to_string();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A section is looked up by its dotted path, and a missing one is null.
    #[test]
    fn configuration_is_answered_by_section() {
        let settings = json!({"rust-analyzer": {"check": {"command": "clippy"}}});
        assert_eq!(
            setting(Some(&settings), Some("rust-analyzer.check.command")),
            json!("clippy")
        );
        assert_eq!(setting(Some(&settings), Some("python")), Value::Null);
        assert_eq!(setting(None, Some("rust-analyzer")), Value::Null);
        assert_eq!(setting(Some(&settings), None), settings);
    }

    /// A provider given as an object or as true counts, false or absent does not, and the
    /// encoding is UTF-16 unless the server names another.
    #[test]
    fn capabilities_are_read_from_the_server_s_answer() {
        let caps = Capabilities::read(&json!({
            "hoverProvider": true,
            "definitionProvider": {"workDoneProgress": true},
            "referencesProvider": false,
            "positionEncoding": "utf-8",
            "textDocumentSync": {"change": 1, "save": {"includeText": false}}
        }));
        assert!(caps.hover && caps.definition && !caps.references && caps.save);
        assert_eq!(caps.encoding, Encoding::Utf8);
        assert_eq!(Capabilities::read(&json!({})).encoding, Encoding::Utf16);
    }
}
