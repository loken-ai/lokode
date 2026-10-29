//! lokode as the agent of an editor, over the Agent Client Protocol.
//!
//! The editor starts `lokode acp` and speaks JSON-RPC to it, one message a line on its
//! standard input and output. Each session the editor opens is a session of its own, built
//! as the terminal builds one, for the project the editor names.
//!
//! | Editor asks | lokode does |
//! |---|---|
//! | `session/new`, `session/load` | opens a session, or comes back to a recorded one and replays it |
//! | `session/prompt` | runs a turn, a `/command` included, and answers once it is over |
//! | `session/set_mode` | acts, accepts edits or plans |
//! | `session/cancel` | stops the turn |
//!
//! lokode asks the editor in turn: before a change, with `session/request_permission`, and
//! for a tool server's form, with `elicitation/create` when the editor offers it. Commands
//! the model runs are run by lokode, in its sandbox, never in a terminal of the editor's.

pub mod updates;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_util::sync::CancellationToken;

use crate::agent::agent_loop::{AgentEvent, Control, Mode};
use crate::app::AppConfig;
use crate::sandbox::approval::{ApprovalDecision, ApprovalPrompt};

/// The protocol's version this agent speaks.
const PROTOCOL_VERSION: u64 = 1;

/// How long the end of a session waits for what it started to stop.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// Builds the configuration of a session for the project at a path.
pub type ConfigFor = Arc<dyn Fn(&Path) -> Result<AppConfig> + Send + Sync>;

/// An error for the editor: a JSON-RPC code and a message.
type Refusal = (i64, String);

const INVALID_PARAMS: i64 = -32602;
const NOT_FOUND: i64 = -32601;
const FAILED: i64 = -32603;

/// The commands of the terminal an editor has no use for: they act on its screen.
const SCREEN_ONLY: &[&str] = &["jobs", "copy", "resume", "help"];

/// The connection to a client: an editor, or the pages of `lokode serve`.
pub struct Conn {
    out: Sink,
    pending: std::sync::Mutex<HashMap<i64, oneshot::Sender<Value>>>,
    next: AtomicI64,
    /// What the editor said it can do.
    client: std::sync::OnceLock<Value>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    config_for: ConfigFor,
}

impl Conn {
    fn send(&self, msg: Value) {
        (self.out)(msg);
    }

    fn notify(&self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    fn update(&self, session: &str, update: Value) {
        self.notify(
            "session/update",
            json!({"sessionId": session, "update": update}),
        );
    }

    fn reply(&self, id: &Value, result: Result<Value, Refusal>) {
        self.send(match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
            }
        });
    }

    /// Ask the editor something and wait for its answer.
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(id, tx);
        }
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let reply = rx.await.map_err(|_| anyhow!("the editor went away"))?;
        if let Some(err) = reply.get("error") {
            return Err(anyhow!("{method}: {err}"));
        }
        Ok(reply.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Whether the editor offers what `pointer` names in its capabilities.
    fn offers(&self, pointer: &str) -> bool {
        self.client
            .get()
            .and_then(|c| c.pointer(pointer))
            .is_some_and(|v| !v.is_null() && v != &json!(false))
    }
}

/// One session the editor opened.
pub struct Session {
    id: String,
    root: PathBuf,
    turns: mpsc::Sender<Turn>,
    control: mpsc::Sender<Control>,
    /// The turn running, to stop it.
    running: std::sync::Mutex<Option<CancellationToken>>,
    /// What is worth saying before the first answer.
    notes: std::sync::Mutex<Vec<String>>,
    shutdown: CancellationToken,
    finished: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    recorder: Mutex<Option<tokio::task::JoinHandle<()>>>,
    lsp: Arc<crate::lsp::LspPool>,
    // What the commands need.
    checkpoints: Option<Arc<crate::checkpoint::Checkpoints>>,
    policy: crate::sandbox::policy::Policy,
    tools: Vec<String>,
    digest: Arc<Mutex<crate::agent::digest::Digest>>,
    mcp: crate::mcp::Clients,
    mcp_servers: std::collections::BTreeMap<String, crate::config::McpServer>,
    approvals: Arc<crate::sandbox::approval::SessionApprovals>,
    mode: std::sync::Mutex<&'static str>,
}

/// A turn to run, and who to tell once its updates are all out, with what it came to.
struct Turn {
    text: String,
    cancel: CancellationToken,
    done: oneshot::Sender<Value>,
}

/// The editor's files, read and written through it when it offers to.
struct EditorFiles {
    conn: Arc<Conn>,
    session: String,
}

#[async_trait::async_trait]
impl crate::tools::registry::Editor for EditorFiles {
    async fn read(&self, path: &Path) -> Option<String> {
        if !self.conn.offers("/fs/readTextFile") {
            return None;
        }
        let read = self
            .conn
            .request(
                "fs/read_text_file",
                json!({"sessionId": self.session, "path": path}),
            )
            .await
            .ok()?;
        read.get("content")?.as_str().map(str::to_string)
    }

    async fn write(&self, path: &Path, contents: &str) -> bool {
        if !self.conn.offers("/fs/writeTextFile") {
            return false;
        }
        self.conn
            .request(
                "fs/write_text_file",
                json!({"sessionId": self.session, "path": path, "content": contents}),
            )
            .await
            .is_ok()
    }
}

/// Serve the editor on standard input and output until it closes them.
/// Where the messages for the client go.
pub type Sink = Box<dyn Fn(Value) + Send + Sync>;

/// A connection to a client, whose messages go to `out`.
pub fn connect(out: Sink, config_for: ConfigFor) -> Arc<Conn> {
    Arc::new(Conn {
        out,
        pending: Default::default(),
        next: AtomicI64::new(1),
        client: Default::default(),
        sessions: Default::default(),
        config_for,
    })
}

/// Take one message of the client's: a request, answered when it is done, a
/// notification, or the answer to a question of lokode's.
pub fn dispatch(conn: &Arc<Conn>, msg: Value) {
    let method = msg
        .get("method")
        .and_then(|m| m.as_str())
        .map(str::to_string);
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    match (msg.get("id").cloned(), method) {
        (Some(id), Some(method)) => {
            let conn = conn.clone();
            tokio::spawn(async move {
                let result = handle(&conn, &method, &params).await;
                let opened = match (&result, method.as_str()) {
                    (Ok(r), "session/new") => r.get("sessionId").cloned(),
                    (Ok(_), "session/load") => params.get("sessionId").cloned(),
                    _ => None,
                };
                conn.reply(&id, result);
                // Offered once the client knows the session.
                if let Some(opened) = opened {
                    if let Ok(session) = session_of(&conn, &json!({"sessionId": opened})).await {
                        conn.update(
                            &session.id,
                            json!({"sessionUpdate": "available_commands_update", "availableCommands": commands_of(&session)}),
                        );
                    }
                }
            });
        }
        (None, Some(method)) => {
            let conn = conn.clone();
            tokio::spawn(async move { notified(&conn, &method, &params).await });
        }
        (Some(id), None) => {
            let waiting = id
                .as_i64()
                .and_then(|id| conn.pending.lock().ok()?.remove(&id));
            if let Some(tx) = waiting {
                let _ = tx.send(msg);
            }
        }
        (None, None) => {}
    }
}

/// Close every session of the connection, their transcripts written out.
pub async fn close(conn: &Arc<Conn>) {
    let sessions: Vec<Arc<Session>> = conn.sessions.lock().await.values().cloned().collect();
    for session in sessions {
        session.close().await;
    }
}

/// Serve the editor on standard input and output until it closes them.
pub async fn run(config_for: ConfigFor) -> Result<()> {
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(msg) = out_rx.recv().await {
            let mut line = serde_json::to_vec(&msg).unwrap_or_default();
            line.push(b'\n');
            if stdout.write_all(&line).await.is_err() || stdout.flush().await.is_err() {
                break;
            }
        }
    });
    let conn = connect(
        Box::new(move |msg| {
            let _ = out_tx.send(msg);
        }),
        config_for,
    );
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(&line) {
            Ok(msg) => dispatch(&conn, msg),
            Err(_) => tracing::warn!("an unreadable line from the editor"),
        }
    }
    close(&conn).await;
    drop(conn);
    let _ = tokio::time::timeout(SHUTDOWN_GRACE, writer).await;
    Ok(())
}

/// Answer one request of the editor's.
async fn handle(conn: &Arc<Conn>, method: &str, params: &Value) -> Result<Value, Refusal> {
    match method {
        "initialize" => {
            let _ = conn.client.set(
                params
                    .get("clientCapabilities")
                    .cloned()
                    .unwrap_or(Value::Null),
            );
            Ok(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "agentCapabilities": {
                    "loadSession": true,
                    "promptCapabilities": {"image": false, "audio": false, "embeddedContext": true},
                    "mcpCapabilities": {"http": true, "sse": false},
                },
                "authMethods": [],
                "agentInfo": {"name": "lokode", "title": "lokode", "version": env!("CARGO_PKG_VERSION")},
            }))
        }
        "authenticate" => Ok(json!({})),
        "session/new" | "session/load" => {
            let cwd = params
                .get("cwd")
                .and_then(|c| c.as_str())
                .ok_or((INVALID_PARAMS, "a session needs its cwd".to_string()))?;
            let resume = params
                .get("sessionId")
                .and_then(|s| s.as_str())
                .map(str::to_string);
            // A session already open is not opened twice: its pages attach to it.
            if let Some(id) = &resume {
                if let Some(open) = conn.sessions.lock().await.get(id).cloned() {
                    let current = open.mode.lock().map(|m| *m).unwrap_or("act");
                    return Ok(json!({"modes": updates::modes(current)}));
                }
            }
            let servers = params.get("mcpServers").cloned().unwrap_or(Value::Null);
            let (session, restored) = open(conn, Path::new(cwd), resume, &servers)
                .await
                .map_err(|e| (FAILED, format!("{e:#}")))?;
            for m in restored {
                let kind = match m.role.as_str() {
                    "user" => "user_message_chunk",
                    "assistant" if !m.content.trim().is_empty() => "agent_message_chunk",
                    _ => continue,
                };
                conn.update(
                    &session.id,
                    json!({"sessionUpdate": kind, "content": {"type": "text", "text": m.content}}),
                );
            }
            let current = session.mode.lock().map(|m| *m).unwrap_or("act");
            let modes = updates::modes(current);
            Ok(match method {
                "session/new" => json!({"sessionId": session.id, "modes": modes}),
                _ => json!({"modes": modes}),
            })
        }
        "session/prompt" => {
            let session = session_of(conn, params).await?;
            let text = prompt_text(params, &session.root);
            session.prompt(conn, text).await
        }
        "session/set_mode" => {
            let session = session_of(conn, params).await?;
            let id = params
                .get("modeId")
                .and_then(|m| m.as_str())
                .unwrap_or_default();
            session.set_mode(id).await?;
            Ok(json!({}))
        }
        other => Err((NOT_FOUND, format!("lokode does not answer {other}"))),
    }
}

/// Act on a notification of the editor's.
async fn notified(conn: &Arc<Conn>, method: &str, params: &Value) {
    if method == "session/cancel" {
        if let Ok(session) = session_of(conn, params).await {
            if let Some(cancel) = session.running.lock().ok().and_then(|r| r.clone()) {
                cancel.cancel();
            }
        }
    }
}

async fn session_of(conn: &Arc<Conn>, params: &Value) -> Result<Arc<Session>, Refusal> {
    let id = params
        .get("sessionId")
        .and_then(|s| s.as_str())
        .unwrap_or_default();
    conn.sessions
        .lock()
        .await
        .get(id)
        .cloned()
        .ok_or((INVALID_PARAMS, format!("there is no session {id}")))
}

/// The prompt as one text: its text blocks, the files it links named with `@`, and the
/// documents it embeds written out.
fn prompt_text(params: &Value, root: &Path) -> String {
    let mut parts = Vec::new();
    for block in params
        .get("prompt")
        .and_then(|p| p.as_array())
        .into_iter()
        .flatten()
    {
        let text = |v: Option<&Value>| v.and_then(|t| t.as_str()).unwrap_or_default().to_string();
        match block.get("type").and_then(|t| t.as_str()) {
            Some("text") => parts.push(text(block.get("text"))),
            Some("resource_link") => {
                let uri = text(block.get("uri"));
                parts.push(mention(&uri, root));
            }
            Some("resource") => {
                let uri = text(block.pointer("/resource/uri"));
                match block.pointer("/resource/text").and_then(|t| t.as_str()) {
                    Some(body) => parts.push(format!("{uri}:\n```\n{body}\n```")),
                    None => parts.push(mention(&uri, root)),
                }
            }
            _ => {}
        }
    }
    parts.join("\n")
}

/// A file the editor links, as the `@` the terminal would have typed.
fn mention(uri: &str, root: &Path) -> String {
    let path = url::Url::parse(uri)
        .ok()
        .and_then(|u| u.to_file_path().ok())
        .unwrap_or_else(|| PathBuf::from(uri));
    let shown = path.strip_prefix(root).unwrap_or(&path);
    format!("@{}", shown.display())
}

/// The tool servers the editor hands the session, as the configuration writes them.
fn editor_servers(servers: &Value) -> Vec<(String, crate::config::McpServer)> {
    let pairs = |v: Option<&Value>| -> Vec<(String, String)> {
        v.and_then(|e| e.as_array())
            .into_iter()
            .flatten()
            .filter_map(|e| {
                Some((
                    e.get("name")?.as_str()?.to_string(),
                    e.get("value")?.as_str()?.to_string(),
                ))
            })
            .collect()
    };
    servers
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let name = s.get("name")?.as_str()?.to_string();
            let text = |k: &str| s.get(k).and_then(|v| v.as_str()).map(str::to_string);
            let args = s
                .get("args")
                .and_then(|a| a.as_array())
                .into_iter()
                .flatten()
                .filter_map(|a| a.as_str().map(str::to_string))
                .collect();
            let spec = crate::config::McpServer {
                command: text("command"),
                args,
                url: text("url"),
                headers: pairs(s.get("headers")).into_iter().collect(),
                token_env: None,
                client_id: None,
                scopes: Vec::new(),
                env: pairs(s.get("env")).into_iter().collect(),
                enabled: true,
                timeout: None,
            };
            Some((name, spec))
        })
        .collect()
}

/// Open a session for the project at `cwd`, or come back to `resume`. Gives the session and
/// the conversation it comes back with.
async fn open(
    conn: &Arc<Conn>,
    cwd: &Path,
    resume: Option<String>,
    servers: &Value,
) -> Result<(Arc<Session>, Vec<crate::backend::wire::Message>)> {
    let root = cwd
        .canonicalize()
        .map_err(|e| anyhow!("{}: {e}", cwd.display()))?;
    let mut cfg = (conn.config_for)(&root)?;
    cfg.headless = false;
    cfg.resume = resume.map(crate::app::Resume::Id);
    for (name, spec) in editor_servers(servers) {
        cfg.mcp_servers.entry(name).or_insert(spec);
    }
    let booted = crate::boot::boot(&cfg).await?;
    let crate::boot::Booted {
        mut agent,
        lsp,
        mut approval_rx,
        shutdown,
        recorder,
        session_id,
        instruction_notes,
        mcp,
        mcp_clients,
        tools,
        tasks,
        digest,
        checkpoints,
        checkpoint_error,
        session_approvals,
        restored,
        mut live,
        mut questions,
        protocol_cell,
        ..
    } = booted;
    session_approvals.accept_writes(cfg.accept_writes);

    let mut notes: Vec<String> = cfg.warnings.clone();
    notes.extend(instruction_notes);
    notes.extend(mcp.notices.clone());
    notes.extend(checkpoint_error.map(|e| format!("no change history: {e}")));

    let (turn_tx, mut turn_rx) = mpsc::channel::<Turn>(4);
    let (control_tx, mut control_rx) = mpsc::channel::<Control>(4);
    let (evt_tx, mut evt_rx) = mpsc::channel::<AgentEvent>(128);
    let (finish_tx, mut finish_rx) = mpsc::channel::<oneshot::Sender<Value>>(4);
    agent.ctx.editor = Some(Arc::new(EditorFiles {
        conn: conn.clone(),
        session: session_id.clone(),
    }));

    let mode = updates::mode_id(agent.cfg.mode, cfg.accept_writes);
    let session = Arc::new(Session {
        id: session_id.clone(),
        root: root.clone(),
        turns: turn_tx,
        control: control_tx.clone(),
        running: Default::default(),
        notes: std::sync::Mutex::new(notes),
        shutdown: shutdown.clone(),
        finished: Default::default(),
        recorder: Mutex::new(recorder),
        lsp,
        checkpoints,
        policy: cfg.policy.clone(),
        tools,
        digest,
        mcp: mcp_clients,
        mcp_servers: cfg.mcp_servers.clone(),
        approvals: session_approvals,
        mode: std::sync::Mutex::new(mode),
    });

    // Whether the backend really makes tool calls, asked once as the terminal asks it.
    if cfg.tool_protocol == crate::app::ProtocolChoice::Auto {
        let backend = agent.backend.clone();
        let model = agent.cfg.model.clone();
        let (control, token) = (control_tx.clone(), shutdown.child_token());
        tokio::spawn(async move {
            if crate::backend::probe::ignores_native_tools(backend, &model, token).await {
                protocol_cell.set(crate::agent::protocol::ToolProtocol::Envelope);
                let _ = control
                    .send(Control::Protocol(
                        crate::agent::protocol::ToolProtocol::Envelope,
                    ))
                    .await;
            }
        });
    }

    let mut handles = Vec::new();
    // The agent: a turn at a time, the commands' requests between turns.
    {
        let (evt_tx, finish_tx, stop) = (evt_tx.clone(), finish_tx.clone(), shutdown.clone());
        handles.push(tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = stop.cancelled() => break,
                    turn = turn_rx.recv() => {
                        let Some(Turn { text, cancel, done }) = turn else { break };
                        if let Err(e) = agent.run_user_turn(text, cancel, evt_tx.clone()).await {
                            let _ = evt_tx.send(AgentEvent::Notice(format!("agent error: {e:#}"))).await;
                        }
                        // After every update of the turn, which are already queued.
                        let _ = finish_tx.send(done).await;
                    }
                    request = control_rx.recv() => {
                        let Some(request) = request else { break };
                        agent.control(request, &evt_tx).await;
                    }
                    // What an agent working in the background answered while no prompt
                    // ran: the session takes it up, its updates reaching the client.
                    said = agent.steer.recv() => {
                        let Some(said) = said else { continue };
                        if let Err(e) = agent
                            .run_user_turn(said, stop.child_token(), evt_tx.clone())
                            .await
                        {
                            let _ = evt_tx.send(AgentEvent::Notice(format!("agent error: {e:#}"))).await;
                        }
                    }
                }
            }
            agent.end_session().await;
        }));
    }
    // The updates, in the order the engine said them.
    {
        let (conn, id, root) = (conn.clone(), session_id.clone(), root.clone());
        let tasks = tasks.clone();
        let session = session.clone();
        handles.push(tokio::spawn(async move {
            let mut translator = updates::Translator::new(&root);
            let mut plan = tasks.get().await;
            loop {
                tokio::select! {
                    biased;
                    event = evt_rx.recv() => {
                        let Some(event) = event else { break };
                        if let AgentEvent::Mode(m) = &event {
                            if let Ok(mut mode) = session.mode.lock() {
                                *mode = updates::mode_id(*m, false);
                            }
                        }
                        let settled = matches!(event, AgentEvent::ToolResult { .. });
                        for update in translator.apply(&event) {
                            conn.update(&id, update);
                        }
                        if settled {
                            send_plan(&conn, &id, &tasks, &mut plan).await;
                        }
                    }
                    done = finish_rx.recv() => {
                        let Some(done) = done else { break };
                        while let Ok(event) = evt_rx.try_recv() {
                            for update in translator.apply(&event) {
                                conn.update(&id, update);
                            }
                        }
                        send_plan(&conn, &id, &tasks, &mut plan).await;
                        let _ = done.send(translator.meta());
                    }
                }
            }
        }));
    }
    // The approvals, asked of the editor.
    {
        let (conn, id, root) = (conn.clone(), session_id.clone(), root.clone());
        handles.push(tokio::spawn(async move {
            let mut asked = 0u64;
            while let Some(prompt) = approval_rx.recv().await {
                asked += 1;
                let (conn, id, root) = (conn.clone(), id.clone(), root.clone());
                tokio::spawn(async move { permission(&conn, &id, &root, prompt, asked).await });
            }
        }));
    }
    // The tool servers' forms, put to the editor when it can show one.
    {
        let (conn, id) = (conn.clone(), session_id.clone());
        handles.push(tokio::spawn(async move {
            while let Some(q) = questions.recv().await {
                let (conn, id) = (conn.clone(), id.clone());
                tokio::spawn(async move { elicit(&conn, &id, q).await });
            }
        }));
    }
    // What commands print as they run reaches the editor with their result.
    handles.push(tokio::spawn(
        async move { while live.recv().await.is_some() {} },
    ));
    *session.finished.lock().await = handles;

    conn.sessions
        .lock()
        .await
        .insert(session_id, session.clone());
    Ok((session, restored))
}

/// Send the task list when it changed since `sent`.
async fn send_plan(
    conn: &Conn,
    id: &str,
    tasks: &crate::tools::plan::TaskList,
    sent: &mut Vec<crate::tools::plan::Task>,
) {
    let now = tasks.get().await;
    if &now != sent {
        conn.update(id, updates::plan(&now));
        *sent = now;
    }
}

/// Ask the editor for the approval `prompt`, the `n`th of the session.
async fn permission(conn: &Conn, session: &str, root: &Path, prompt: ApprovalPrompt, n: u64) {
    let ApprovalPrompt { req, reply, call } = prompt;
    let mut content = Vec::new();
    if let Some(diff) = &req.diff_preview {
        content.push(json!({"type": "content", "content": {"type": "text", "text": format!("```diff\n{diff}\n```")}}));
    }
    let tool_call = json!({
        "toolCallId": call.unwrap_or_else(|| format!("approval-{n}")),
        "title": req.summary,
        "kind": updates::kind(&req.tool),
        "status": "pending",
        "content": content,
    });
    let always = format!("Always: {}", req.scope.describe_under(root));
    let asked = conn
        .request(
            "session/request_permission",
            json!({
                "sessionId": session,
                "toolCall": tool_call,
                "options": [
                    {"optionId": "once", "name": "Allow once", "kind": "allow_once"},
                    {"optionId": "always", "name": always, "kind": "allow_always"},
                    {"optionId": "no", "name": "Reject", "kind": "reject_once"},
                ],
            }),
        )
        .await;
    let decision = match asked
        .ok()
        .as_ref()
        .and_then(|r| r.pointer("/outcome/optionId"))
        .and_then(|o| o.as_str())
    {
        Some("once") => ApprovalDecision::Once,
        Some("always") => ApprovalDecision::Always,
        _ => ApprovalDecision::Deny(None),
    };
    let _ = reply.send(decision);
}

/// Put a tool server's form to the editor, or decline it when the editor cannot show one.
async fn elicit(conn: &Conn, session: &str, q: crate::mcp::host::Question) {
    use crate::mcp::host::Answer;
    if !conn.offers("/elicitation/form") {
        let _ = q.reply.send(Answer::Decline);
        return;
    }
    let asked = conn
        .request(
            "elicitation/create",
            json!({
                "sessionId": session,
                "mode": "form",
                "message": format!("{} asks: {}", q.server, q.message),
                "requestedSchema": crate::mcp::host::schema(&q.fields),
            }),
        )
        .await;
    let answer = match asked {
        Ok(r) if r.get("action").and_then(|a| a.as_str()) == Some("accept") => Answer::Accept(
            r.get("content")
                .and_then(|c| c.as_object())
                .cloned()
                .unwrap_or_default(),
        ),
        _ => Answer::Decline,
    };
    let _ = q.reply.send(answer);
}

/// The commands an editor offers after `/`: the terminal's, but those for its screen, and
/// those the project and the user wrote.
fn commands_of(session: &Session) -> Vec<Value> {
    let mut out: Vec<Value> = crate::ui::commands::HELP
        .lines()
        .filter_map(|line| {
            let line = line.strip_prefix('/')?;
            let (name, about) = line.split_once(char::is_whitespace)?;
            (!SCREEN_ONLY.contains(&name))
                .then(|| json!({"name": name, "description": about.trim()}))
        })
        .collect();
    for c in crate::ui::commands::written_commands(&session.root) {
        let mut entry = json!({"name": c.name, "description": c.description});
        if let Some(hint) = c.argument_hint {
            entry["input"] = json!({"hint": hint});
        }
        out.push(entry);
    }
    out
}

impl Session {
    /// Run what the editor sent: a command, or a turn. Answers with why it stopped.
    async fn prompt(&self, conn: &Arc<Conn>, text: String) -> Result<Value, Refusal> {
        let notes = self
            .notes
            .lock()
            .map(|mut n| std::mem::take(&mut *n))
            .unwrap_or_default();
        if !notes.is_empty() {
            let text = notes.iter().map(|n| format!("{n}\n")).collect::<String>();
            conn.update(
                &self.id,
                json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": text}}),
            );
        }
        let text = match text.trim().strip_prefix('/') {
            Some(command) => match self.command(conn, command).await {
                Some(prompt) => prompt,
                None => return Ok(json!({"stopReason": "end_turn"})),
            },
            None => text,
        };
        let cancel = self.shutdown.child_token();
        if let Ok(mut running) = self.running.lock() {
            *running = Some(cancel.clone());
        }
        let (done, finished) = oneshot::channel();
        let sent = self
            .turns
            .send(Turn {
                text,
                cancel: cancel.clone(),
                done,
            })
            .await;
        let meta = match sent {
            Ok(()) => finished.await.unwrap_or(Value::Null),
            Err(_) => Value::Null,
        };
        if let Ok(mut running) = self.running.lock() {
            *running = None;
        }
        let reason = match cancel.is_cancelled() {
            true => "cancelled",
            false => "end_turn",
        };
        Ok(json!({"stopReason": reason, "_meta": meta}))
    }

    /// Run a command as the terminal would. Gives the prompt it makes, when it makes one.
    async fn command(&self, conn: &Arc<Conn>, command: &str) -> Option<String> {
        use crate::ui::commands::{run_command, CommandCtx, Ran};
        let say = |text: &str| {
            conn.update(
                &self.id,
                json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}),
            )
        };
        let busy = self.running.lock().map(|r| r.is_some()).unwrap_or(false);
        let mcp = crate::mcp::snapshot(&self.mcp);
        let ran = run_command(
            command,
            CommandCtx {
                checkpoints: self.checkpoints.as_deref(),
                root: &self.root,
                control: &self.control,
                policy: &self.policy,
                tools: &self.tools,
                digest: &self.digest,
                mcp: &mcp,
                mcp_servers: &self.mcp_servers,
                session_id: &self.id,
                approvals: &self.approvals,
                busy,
            },
        )
        .await;
        match ran {
            Ran::Prompt(prompt) => return Some(prompt),
            Ran::Said(text) | Ran::Cleared(text) => say(&text),
            Ran::Choose(chooser) => {
                let mut text = format!("{}:\n\n", chooser.title());
                for c in chooser.shown() {
                    text.push_str(&format!(
                        "- `/{} {}` {}\n",
                        chooser.verb(),
                        c.value,
                        c.about
                    ));
                }
                say(&text);
            }
            Ran::Resumed(_) => say("the session now carries on another one recorded earlier"),
            Ran::Later { said, then } => {
                say(&said);
                let (conn, id) = (conn.clone(), self.id.clone());
                tokio::spawn(async move {
                    let text = then.await;
                    conn.update(
                        &id,
                        json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}),
                    );
                });
            }
        }
        // The mode follows what a command changed.
        if command.starts_with("plan") || command.starts_with("act") {
            let id = if command.starts_with("plan") {
                "plan"
            } else {
                "act"
            };
            if let Ok(mut mode) = self.mode.lock() {
                *mode = id;
            }
            conn.update(
                &self.id,
                json!({"sessionUpdate": "current_mode_update", "currentModeId": id}),
            );
        }
        None
    }

    /// Switch to the mode the editor named.
    async fn set_mode(&self, id: &str) -> Result<(), Refusal> {
        let (mode, writes) = match id {
            "act" => (Mode::Act, false),
            "accept-edits" => (Mode::Act, true),
            "plan" => (Mode::Plan, false),
            other => return Err((INVALID_PARAMS, format!("lokode has no mode {other}"))),
        };
        let (tx, rx) = oneshot::channel();
        self.control
            .send(Control::Mode { mode, reply: tx })
            .await
            .map_err(|_| (FAILED, "the agent is not listening".to_string()))?;
        let _ = rx.await;
        self.approvals.accept_writes(writes);
        if let Ok(mut current) = self.mode.lock() {
            *current = updates::mode_id(mode, writes);
        }
        Ok(())
    }

    /// Stop everything the session started, and let its transcript reach the disk.
    async fn close(&self) {
        self.shutdown.cancel();
        let _ = tokio::time::timeout(SHUTDOWN_GRACE, self.lsp.shutdown()).await;
        for client in crate::mcp::snapshot(&self.mcp) {
            client.shutdown().await;
        }
        for handle in self.finished.lock().await.drain(..) {
            handle.abort();
        }
        if let Some(recorder) = self.recorder.lock().await.take() {
            let _ = tokio::time::timeout(SHUTDOWN_GRACE, recorder).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Linked files become mentions under the project, and embedded ones are written out.
    #[test]
    fn a_prompt_is_read_as_text() {
        let params = json!({"prompt": [
            {"type": "text", "text": "fix"},
            {"type": "resource_link", "name": "a", "uri": "file:///p/src/a.rs"},
            {"type": "resource", "resource": {"uri": "file:///p/b.rs", "text": "fn b() {}"}},
        ]});
        assert_eq!(
            prompt_text(&params, Path::new("/p")),
            "fix\n@src/a.rs\nfile:///p/b.rs:\n```\nfn b() {}\n```"
        );
    }

    /// The editor's servers are read in both forms it writes them.
    #[test]
    fn the_editors_servers_are_read() {
        let servers = editor_servers(&json!([
            {"name": "fs", "command": "mcp-fs", "args": ["--stdio"], "env": [{"name": "K", "value": "v"}]},
            {"type": "http", "name": "api", "url": "https://x/mcp", "headers": [{"name": "Authorization", "value": "Bearer t"}]},
        ]));
        assert_eq!(servers[0].0, "fs");
        assert_eq!(servers[0].1.command.as_deref(), Some("mcp-fs"));
        assert_eq!(servers[0].1.env["K"], "v");
        assert_eq!(servers[1].1.url.as_deref(), Some("https://x/mcp"));
        assert_eq!(servers[1].1.headers["Authorization"], "Bearer t");
    }
}
