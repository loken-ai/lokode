//! Minimal LSP stdio client. Spawns one language server per detected
//! file extension and reuses it for the lifetime of the TUI session.
//!
//! Scope: initialize → didOpen/didChange → hover, pullDiagnostics → shutdown. Enough for
//! the agent to surface compile errors and type info, and to keep surfacing them after it
//! has edited the file itself.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use lsp_types::{
    notification::{DidChangeTextDocument, DidOpenTextDocument, Initialized, Notification},
    request::{HoverRequest, Initialize, Request as LspRequest},
    ClientCapabilities, DidChangeTextDocumentParams, DidOpenTextDocumentParams, HoverParams,
    InitializeParams, InitializedParams, Position, TextDocumentContentChangeEvent,
    TextDocumentIdentifier, TextDocumentItem, TextDocumentPositionParams, Url,
    VersionedTextDocumentIdentifier, WorkDoneProgressParams,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{oneshot, Mutex};

use crate::sandbox::allowlist;
use crate::tools::registry::{Tool, ToolCtx, ToolOutput};

// --------------------------------------------------------------- per-lang --

#[derive(Debug, Clone, Copy)]
enum LangId {
    Rust,
    Python,
    TypeScript,
    JavaScript,
}

impl LangId {
    fn from_path(p: &Path) -> Option<Self> {
        match p.extension().and_then(|s| s.to_str()) {
            Some("rs") => Some(LangId::Rust),
            Some("py") => Some(LangId::Python),
            Some("ts") | Some("tsx") => Some(LangId::TypeScript),
            Some("js") | Some("jsx") | Some("mjs") => Some(LangId::JavaScript),
            _ => None,
        }
    }
    fn language_id(self) -> &'static str {
        match self {
            LangId::Rust => "rust",
            LangId::Python => "python",
            LangId::TypeScript => "typescript",
            LangId::JavaScript => "javascript",
        }
    }
    fn server(self) -> (String, Vec<String>) {
        let (program, args): (&str, &[&str]) = match self {
            LangId::Rust => ("rust-analyzer", &[]),
            LangId::Python => ("pyright-langserver", &["--stdio"]),
            LangId::TypeScript | LangId::JavaScript => ("typescript-language-server", &["--stdio"]),
        };
        // An override for a server that is not on the path under its usual name, or is
        // there under it and does not work - a rustup shim for a component that was never
        // installed answers to `rust-analyzer` and fails.
        let var = format!("VERVE_LSP_{}", self.language_id().to_uppercase());
        match std::env::var(&var) {
            Ok(cmd) if !cmd.trim().is_empty() => {
                let mut parts = cmd.split_whitespace().map(String::from);
                let program = parts.next().unwrap_or_else(|| program.to_string());
                (program, parts.collect())
            }
            _ => (
                program.to_string(),
                args.iter().map(|s| s.to_string()).collect(),
            ),
        }
    }
}

// ----------------------------------------------------------------- client --

/// What was last sent to the server for one file.
struct OpenDoc {
    version: i32,
    text: String,
}

/// Bytes of the server's own complaints to keep for when something goes wrong.
const STDERR_TAIL: usize = 4 * 1024;

struct LspProcess {
    _child: Child,
    stdin: Mutex<ChildStdin>,
    next_id: AtomicI64,
    pending: Mutex<HashMap<i64, oneshot::Sender<Value>>>,
    diagnostics: Mutex<HashMap<Url, Vec<lsp_types::Diagnostic>>>,
    opened: Mutex<HashMap<PathBuf, OpenDoc>>,
    /// The tail of the server's stderr.
    ///
    /// It used to go to `/dev/null`, so a language server that refused to start - a
    /// missing toolchain, an unreadable project file - reported nothing at all and the
    /// tool simply returned no diagnostics, which reads like a file with no problems.
    stderr: Arc<Mutex<String>>,
}

impl LspProcess {
    async fn spawn(lang: LangId, root: &Path) -> Result<Arc<Self>> {
        let (program, args) = lang.server();
        let mut child = tokio::process::Command::new(&program)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| anyhow!("spawn {program}: {e}"))?;

        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;

        let stderr_buf = Arc::new(Mutex::new(String::new()));
        if let Some(stderr) = child.stderr.take() {
            let buf = stderr_buf.clone();
            tokio::spawn(async move { drain_stderr(stderr, buf).await });
        }

        let proc = Arc::new(Self {
            _child: child,
            stdin: Mutex::new(stdin),
            next_id: AtomicI64::new(1),
            pending: Mutex::new(HashMap::new()),
            diagnostics: Mutex::new(HashMap::new()),
            opened: Mutex::new(HashMap::new()),
            stderr: stderr_buf,
        });

        // Spawn the reader task.
        {
            let proc = proc.clone();
            tokio::spawn(async move { reader_loop(proc, stdout).await });
        }

        let root_uri = Url::from_directory_path(root).map_err(|_| anyhow!("bad root url"))?;
        #[allow(deprecated)] // root_path / root_uri deprecated but many servers still want them
        let init = InitializeParams {
            process_id: Some(std::process::id()),
            root_uri: Some(root_uri.clone()),
            initialization_options: None,
            capabilities: ClientCapabilities::default(),
            trace: None,
            workspace_folders: Some(vec![lsp_types::WorkspaceFolder {
                uri: root_uri,
                name: "root".into(),
            }]),
            client_info: Some(lsp_types::ClientInfo {
                name: "verve".into(),
                version: Some("0.1".into()),
            }),
            locale: None,
            root_path: None,
        };
        let init_result: Result<Value> = proc
            .request(Initialize::METHOD, serde_json::to_value(init)?)
            .await;
        if let Err(e) = init_result {
            return Err(anyhow!(
                "{program} did not initialise: {e}{}",
                proc.stderr_tail().await
            ));
        }
        proc.notify(
            Initialized::METHOD,
            serde_json::to_value(InitializedParams {})?,
        )
        .await?;
        Ok(proc)
    }

    /// What the server has said on stderr, formatted for appending to an error.
    async fn stderr_tail(&self) -> String {
        let buf = self.stderr.lock().await;
        if buf.trim().is_empty() {
            String::new()
        } else {
            format!("\nserver said:\n{}", buf.trim())
        }
    }

    /// Ask the server to stop, and wait briefly for it to agree.
    ///
    /// Being killed on drop is enough to reclaim the process, but a server given no
    /// chance to shut down can leave its own caches half-written.
    async fn shutdown(&self) {
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            self.request("shutdown", Value::Null),
        )
        .await;
        let _ = self.notify("exit", Value::Null).await;
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let frame = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        write_frame(&self.stdin, &frame).await?;
        let reply = tokio::time::timeout(std::time::Duration::from_secs(30), rx)
            .await
            .map_err(|_| anyhow!("lsp {method} timed out"))?
            .map_err(|_| anyhow!("lsp {method} dropped"))?;
        if let Some(err) = reply.get("error") {
            return Err(anyhow!("lsp {method}: {err}"));
        }
        Ok(reply.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let frame = json!({"jsonrpc": "2.0", "method": method, "params": params});
        write_frame(&self.stdin, &frame).await
    }

    /// Make the server's copy of a file match the one on disk.
    ///
    /// This used to open a file once and never mention it again, so the server kept
    /// answering about the version it first read: after the agent's own first edit, every
    /// diagnostic for the rest of the session described code that no longer existed.
    /// Stale diagnostics are worse than none, because they read as current.
    ///
    /// Syncing from disk rather than from a notification sent by whoever wrote the file
    /// also covers the cases the agent did not cause - the user's editor, a formatter, a
    /// build script.
    async fn sync(&self, path: &Path, lang: LangId) -> Result<(Url, bool)> {
        let uri = Url::from_file_path(path).map_err(|_| anyhow!("bad file url"))?;
        let text = tokio::fs::read_to_string(path).await?;
        let mut opened = self.opened.lock().await;

        match opened.get_mut(path) {
            None => {
                let params = DidOpenTextDocumentParams {
                    text_document: TextDocumentItem {
                        uri: uri.clone(),
                        language_id: lang.language_id().to_string(),
                        version: 1,
                        text: text.clone(),
                    },
                };
                self.notify(DidOpenTextDocument::METHOD, serde_json::to_value(params)?)
                    .await?;
                opened.insert(path.to_path_buf(), OpenDoc { version: 1, text });
                Ok((uri, true))
            }
            Some(doc) if doc.text != text => {
                doc.version += 1;
                doc.text = text.clone();
                let params = DidChangeTextDocumentParams {
                    text_document: VersionedTextDocumentIdentifier {
                        uri: uri.clone(),
                        version: doc.version,
                    },
                    // Whole-file replacement: the agent edits by rewriting, and computing
                    // ranges here would be inventing precision the caller never had.
                    content_changes: vec![TextDocumentContentChangeEvent {
                        range: None,
                        range_length: None,
                        text,
                    }],
                };
                tracing::info!(
                    path = %path.display(),
                    version = doc.version,
                    "file changed under the language server; resyncing"
                );
                self.notify(DidChangeTextDocument::METHOD, serde_json::to_value(params)?)
                    .await?;
                Ok((uri, true))
            }
            Some(_) => Ok((uri, false)),
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

async fn write_frame(stdin: &Mutex<ChildStdin>, payload: &Value) -> Result<()> {
    let body = serde_json::to_vec(payload)?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    let mut sin = stdin.lock().await;
    sin.write_all(header.as_bytes()).await?;
    sin.write_all(&body).await?;
    sin.flush().await?;
    Ok(())
}

async fn reader_loop(proc: Arc<LspProcess>, stdout: ChildStdout) {
    let mut reader = BufReader::new(stdout);
    loop {
        // Read headers
        let mut content_length: Option<usize> = None;
        loop {
            let mut header = String::new();
            let mut byte = [0u8; 1];
            // Read a line terminated by \r\n
            loop {
                if reader.read_exact(&mut byte).await.is_err() {
                    return;
                }
                header.push(byte[0] as char);
                if header.ends_with("\r\n") {
                    break;
                }
            }
            if header == "\r\n" {
                break;
            }
            if let Some(rest) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = rest.trim().trim_end_matches("\r\n").parse().ok();
            }
        }
        let Some(len) = content_length else {
            continue;
        };
        let mut buf = vec![0u8; len];
        if reader.read_exact(&mut buf).await.is_err() {
            return;
        }
        let Ok(msg) = serde_json::from_slice::<Value>(&buf) else {
            continue;
        };

        if let Some(id) = msg.get("id").and_then(|v| v.as_i64()) {
            if let Some(tx) = proc.pending.lock().await.remove(&id) {
                let _ = tx.send(msg);
                continue;
            }
        }
        // Notification from server
        if let Some(method) = msg.get("method").and_then(|v| v.as_str()) {
            if method == "textDocument/publishDiagnostics" {
                #[derive(Deserialize)]
                struct Pub {
                    uri: Url,
                    diagnostics: Vec<lsp_types::Diagnostic>,
                }
                if let Some(params) = msg.get("params") {
                    if let Ok(p) = serde_json::from_value::<Pub>(params.clone()) {
                        proc.diagnostics.lock().await.insert(p.uri, p.diagnostics);
                    }
                }
            }
        }
    }
}

// -------------------------------------------------------------- registry --

/// Holds one LSP process per language. Lazy-spawned.
pub struct LspPool {
    root: PathBuf,
    procs: Mutex<HashMap<&'static str, Arc<LspProcess>>>,
}

impl LspPool {
    pub fn new(root: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            root,
            procs: Mutex::new(HashMap::new()),
        })
    }
    async fn get_or_spawn(&self, lang: LangId) -> Result<Arc<LspProcess>> {
        let key = lang.language_id();
        let mut procs = self.procs.lock().await;
        if let Some(p) = procs.get(key) {
            return Ok(p.clone());
        }
        let proc = LspProcess::spawn(lang, &self.root).await?;
        procs.insert(key, proc.clone());
        Ok(proc)
    }

    /// Ask every running server to stop.
    pub async fn shutdown(&self) {
        let procs: Vec<_> = self.procs.lock().await.drain().map(|(_, p)| p).collect();
        for proc in procs {
            proc.shutdown().await;
        }
    }
}

// --------------------------------------------------------------- tools --

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
        let Some(lang) = LangId::from_path(&path) else {
            return Ok(ToolOutput::err(format!(
                "no LSP server configured for {}",
                path.display()
            )));
        };
        let proc = match self.pool.get_or_spawn(lang).await {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::err(format!("spawn LSP failed: {e}"))),
        };
        let (uri, changed) = match proc.sync(&path, lang).await {
            Ok(u) => u,
            Err(e) => return Ok(ToolOutput::err(format!("sync failed: {e}"))),
        };
        // What the server last published describes the file as it was before the change.
        // Keeping it would let the poll below return immediately with diagnostics for
        // code that no longer exists - the exact staleness this sync exists to end.
        if changed {
            proc.diagnostics.lock().await.remove(&uri);
        }
        // Give the server up to 4s to publish.
        for _ in 0..40 {
            if let Some(diags) = proc.diagnostics.lock().await.get(&uri).cloned() {
                if !diags.is_empty() {
                    return Ok(ToolOutput::ok(format_diags(&path, &diags)));
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let snap = proc
            .diagnostics
            .lock()
            .await
            .get(&uri)
            .cloned()
            .unwrap_or_default();
        Ok(ToolOutput::ok(format_diags(&path, &snap)))
    }
}

/// What the language server says about one file, or nothing when it has no server, it
/// cannot be started, or it says nothing within `wait`.
///
/// Written for two callers: the tool a model calls on purpose, and the check that runs
/// after a write. The second must never fail a write, so every path here answers `None`
/// rather than an error.
pub async fn diagnostics_now(
    pool: &LspPool,
    path: &std::path::Path,
    wait: std::time::Duration,
) -> Option<String> {
    let lang = LangId::from_path(path)?;
    let proc = pool.get_or_spawn(lang).await.ok()?;
    let (uri, changed) = proc.sync(path, lang).await.ok()?;
    if changed {
        proc.diagnostics.lock().await.remove(&uri);
    }
    let deadline = std::time::Instant::now() + wait;
    while std::time::Instant::now() < deadline {
        if let Some(diags) = proc.diagnostics.lock().await.get(&uri).cloned() {
            if diags.is_empty() {
                return None;
            }
            return Some(format_diags(path, &diags));
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    None
}

pub struct LspHover {
    pub pool: Arc<LspPool>,
}

#[derive(Deserialize)]
struct HoverArgs {
    path: PathBuf,
    /// 1-indexed line.
    line: u32,
    /// 1-indexed column.
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
        let Some(lang) = LangId::from_path(&path) else {
            return Ok(ToolOutput::err(format!(
                "no LSP server configured for {}",
                path.display()
            )));
        };
        let proc = match self.pool.get_or_spawn(lang).await {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::err(format!("spawn LSP failed: {e}"))),
        };
        // Hover answers about a position, so it is even more sensitive to the server
        // holding an older copy: the line and column mean nothing against the wrong text.
        let (uri, _) = match proc.sync(&path, lang).await {
            Ok(u) => u,
            Err(e) => return Ok(ToolOutput::err(format!("sync failed: {e}"))),
        };
        let params = HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: Position {
                    line: a.line.saturating_sub(1),
                    character: a.column.saturating_sub(1),
                },
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };
        let reply = match proc
            .request(HoverRequest::METHOD, serde_json::to_value(params)?)
            .await
        {
            Ok(r) => r,
            Err(e) => return Ok(ToolOutput::err(format!("hover failed: {e}"))),
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

fn format_diags(path: &Path, diags: &[lsp_types::Diagnostic]) -> String {
    if diags.is_empty() {
        return format!("{}: (no diagnostics)", path.display());
    }
    let mut out = String::new();
    for d in diags {
        let sev = d
            .severity
            .map(|s| format!("{:?}", s))
            .unwrap_or_else(|| "Info".into());
        let line = d.range.start.line + 1;
        let col = d.range.start.character + 1;
        out.push_str(&format!(
            "{sev}: {}:{line}:{col} {}\n",
            path.display(),
            d.message
        ));
    }
    out
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
        send({"jsonrpc": "2.0", "id": mid, "result": {"capabilities": {"hoverProvider": True}}})
    elif method in ("textDocument/didOpen", "textDocument/didChange"):
        doc = msg["params"]["textDocument"]
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
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "contents": {"kind": "plaintext", "value": "stub hover"}}})
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
        std::env::set_var("VERVE_LSP_RUST", format!("python3 {}", script.display()));
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

        // Hover reaches the same server.
        let hover = LspHover { pool: pool.clone() }
            .run(&h.ctx, json!({"path": "a.rs", "line": 1, "column": 4}))
            .await
            .unwrap();
        assert!(!hover.is_error, "{}", hover.content);
        assert!(hover.content.contains("stub hover"), "{}", hover.content);

        // A path outside the root is refused before any of this is reached.
        let escaped = LspDiagnostics { pool: pool.clone() }
            .run(&h.ctx, json!({"path": "../outside.rs"}))
            .await
            .unwrap();
        assert!(escaped.is_error, "{}", escaped.content);

        pool.shutdown().await;
        std::env::remove_var("VERVE_LSP_RUST");
    }
}
