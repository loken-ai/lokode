//! Minimal LSP stdio client. Spawns one language server per detected
//! file extension and reuses it for the lifetime of the TUI session.
//!
//! Scope: initialize → didOpen → hover, pullDiagnostics. Enough for the
//! agent to surface compile errors and type info.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use lsp_types::{
    notification::{DidOpenTextDocument, Initialized, Notification},
    request::{HoverRequest, Initialize, Request as LspRequest},
    ClientCapabilities, DidOpenTextDocumentParams, HoverParams, InitializeParams,
    InitializedParams, Position, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, Url, WorkDoneProgressParams,
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
    fn server(self) -> (&'static str, &'static [&'static str]) {
        match self {
            LangId::Rust => ("rust-analyzer", &[]),
            LangId::Python => ("pyright-langserver", &["--stdio"]),
            LangId::TypeScript | LangId::JavaScript => {
                ("typescript-language-server", &["--stdio"])
            }
        }
    }
}

// ----------------------------------------------------------------- client --

struct LspProcess {
    _child: Child,
    stdin: Mutex<ChildStdin>,
    next_id: AtomicI64,
    pending: Mutex<HashMap<i64, oneshot::Sender<Value>>>,
    diagnostics: Mutex<HashMap<Url, Vec<lsp_types::Diagnostic>>>,
    opened: Mutex<HashMap<PathBuf, i32>>,
}

impl LspProcess {
    async fn spawn(lang: LangId, root: &Path) -> Result<Arc<Self>> {
        let (program, args) = lang.server();
        let mut child = tokio::process::Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| anyhow!("spawn {program}: {e}"))?;

        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;

        let proc = Arc::new(Self {
            _child: child,
            stdin: Mutex::new(stdin),
            next_id: AtomicI64::new(1),
            pending: Mutex::new(HashMap::new()),
            diagnostics: Mutex::new(HashMap::new()),
            opened: Mutex::new(HashMap::new()),
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
        let _: Value = proc
            .request(Initialize::METHOD, serde_json::to_value(init)?)
            .await?;
        proc.notify(Initialized::METHOD, serde_json::to_value(InitializedParams {})?)
            .await?;
        Ok(proc)
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

    async fn ensure_opened(&self, path: &Path, lang: LangId) -> Result<Url> {
        let uri = Url::from_file_path(path).map_err(|_| anyhow!("bad file url"))?;
        let mut opened = self.opened.lock().await;
        if !opened.contains_key(path) {
            let text = tokio::fs::read_to_string(path).await?;
            let params = DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: lang.language_id().to_string(),
                    version: 1,
                    text,
                },
            };
            self.notify(DidOpenTextDocument::METHOD, serde_json::to_value(params)?)
                .await?;
            opened.insert(path.to_path_buf(), 1);
        }
        Ok(uri)
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
        Arc::new(Self { root, procs: Mutex::new(HashMap::new()) })
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
}

// --------------------------------------------------------------- tools --

pub struct LspDiagnostics {
    pub pool: Arc<LspPool>,
}

#[derive(Deserialize)]
struct DiagArgs { path: PathBuf }

#[async_trait]
impl Tool for LspDiagnostics {
    fn name(&self) -> &'static str { "lsp_diagnostics" }
    fn description(&self) -> &'static str {
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
            Err(e) => return Ok(ToolOutput::err(e.to_string())),
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
        let uri = match proc.ensure_opened(&path, lang).await {
            Ok(u) => u,
            Err(e) => return Ok(ToolOutput::err(format!("didOpen failed: {e}"))),
        };
        // Give the server up to 4s to publish.
        for _ in 0..40 {
            if let Some(diags) = proc.diagnostics.lock().await.get(&uri).cloned() {
                if !diags.is_empty() {
                    return Ok(ToolOutput::ok(format_diags(&path, &diags)));
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let snap = proc.diagnostics.lock().await.get(&uri).cloned().unwrap_or_default();
        Ok(ToolOutput::ok(format_diags(&path, &snap)))
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
    /// 1-indexed column.
    column: u32,
}

#[async_trait]
impl Tool for LspHover {
    fn name(&self) -> &'static str { "lsp_hover" }
    fn description(&self) -> &'static str {
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
            Err(e) => return Ok(ToolOutput::err(e.to_string())),
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
        let uri = match proc.ensure_opened(&path, lang).await {
            Ok(u) => u,
            Err(e) => return Ok(ToolOutput::err(format!("didOpen failed: {e}"))),
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
        let reply = match proc.request(HoverRequest::METHOD, serde_json::to_value(params)?).await {
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
        let sev = d.severity.map(|s| format!("{:?}", s)).unwrap_or_else(|| "Info".into());
        let line = d.range.start.line + 1;
        let col = d.range.start.character + 1;
        out.push_str(&format!("{sev}: {}:{line}:{col} {}\n", path.display(), d.message));
    }
    out
}

fn pretty_contents(c: &Value) -> String {
    // Hover contents can be a string, a {kind, value}, or an array.
    if let Some(s) = c.as_str() { return s.to_string(); }
    if let Some(v) = c.get("value").and_then(|v| v.as_str()) { return v.to_string(); }
    if let Some(arr) = c.as_array() {
        return arr.iter().map(pretty_contents).collect::<Vec<_>>().join("\n");
    }
    c.to_string()
}
