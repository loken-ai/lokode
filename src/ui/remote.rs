//! The terminal on a session of a `lokode serve` already running: `lokode attach <url>`.
//!
//! The screen is the one a local session draws.
//! What it reads from the engine comes from the server instead:
//!
//! | The server sends | The screen gets |
//! |---|---|
//! | a session update | the engine's event it stands for |
//! | the end of a turn | the receipt, the window and the turn's end |
//! | a permission request | an approval, answered back |
//! | a tool server's form | its questions, answered back |
//!
//! What is typed goes to the server as a prompt, a `/command` included.
//! A command runs where the session lives.
//! Typing during a turn queues a prompt there.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures::StreamExt;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::agent::agent_loop::{AgentEvent, Control, Mode, Origin, Receipt};
use crate::sandbox::approval::{ApprovalDecision, ApprovalPrompt, ApprovalRequest, ApprovalScope};
use crate::tools::registry::ToolOutput;

/// How long a request of the screen's waits for the server's answer.
const ANSWER_WAIT: Duration = Duration::from_secs(120);

/// A connection to a `lokode serve`.
pub(crate) struct Remote {
    http: reqwest::Client,
    base: String,
    cookie: String,
    tab: String,
    counter: AtomicU64,
    waiting: Mutex<HashMap<String, oneshot::Sender<Value>>>,
    /// The session followed.
    pub(crate) session: std::sync::OnceLock<String>,
    /// The prompt this screen sent last, so it is not drawn twice when the server echoes it.
    sent: std::sync::Mutex<Option<String>>,
}

impl Remote {
    /// A connection to the server at `url`, the address `lokode serve` printed.
    pub(crate) fn new(url: &str) -> Result<Arc<Self>> {
        let parsed = url::Url::parse(url).context("the address lokode serve printed")?;
        let token = parsed
            .query_pairs()
            .find(|(k, _)| k == "token")
            .map(|(_, v)| v.to_string())
            .ok_or_else(|| {
                anyhow!("{url} carries no token: give the whole address lokode serve printed")
            })?;
        let base = parsed.origin().ascii_serialization();
        Ok(Arc::new(Self {
            http: reqwest::Client::builder().build()?,
            base,
            cookie: format!("lokode-token={token}"),
            tab: format!("tui{}", std::process::id()),
            counter: AtomicU64::new(1),
            waiting: Default::default(),
            session: Default::default(),
            sent: Default::default(),
        }))
    }

    fn id(&self) -> String {
        format!(
            "{}-{}",
            self.tab,
            self.counter.fetch_add(1, Ordering::Relaxed)
        )
    }

    async fn post(&self, msg: Value) -> Result<Value> {
        let resp = self
            .http
            .post(format!("{}/rpc", self.base))
            .header("Cookie", &self.cookie)
            .json(&msg)
            .send()
            .await
            .context("reaching lokode serve")?;
        if !resp.status().is_success() {
            anyhow::bail!("lokode serve answered {}", resp.status());
        }
        Ok(resp.json().await?)
    }

    /// A request of lokode's own, answered in the response.
    async fn own(&self, method: &str, params: Value) -> Result<Value> {
        let got = self
            .post(json!({"jsonrpc": "2.0", "id": self.id(), "method": method, "params": params}))
            .await?;
        got.get("result")
            .cloned()
            .ok_or_else(|| anyhow!("{method}: lokode serve did not answer"))
    }

    /// A protocol request, answered on the stream.
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.id();
        let (tx, rx) = oneshot::channel();
        self.waiting.lock().await.insert(id.clone(), tx);
        self.post(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        let reply = tokio::time::timeout(ANSWER_WAIT, rx)
            .await
            .map_err(|_| anyhow!("{method}: no answer"))??;
        if let Some(err) = reply.get("error") {
            anyhow::bail!("{method}: {err}");
        }
        Ok(reply.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn notify(&self, method: &str, params: Value) {
        let _ = self
            .post(json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await;
    }

    async fn answer(&self, id: &Value, result: Value) {
        let _ = self
            .post(json!({"jsonrpc": "2.0", "id": id, "result": result}))
            .await;
    }

    fn session_id(&self) -> String {
        self.session.get().cloned().unwrap_or_default()
    }
}

/// What the server says about itself and its sessions.
pub(crate) struct Hello {
    pub(crate) root: std::path::PathBuf,
    pub(crate) open: Vec<String>,
}

pub(crate) async fn hello(remote: &Remote) -> Result<Hello> {
    let got = remote.own("_lokode/hello", json!({})).await?;
    Ok(Hello {
        root: got
            .get("root")
            .and_then(|r| r.as_str())
            .map(std::path::PathBuf::from)
            .unwrap_or_default(),
        open: got
            .get("open")
            .and_then(|o| o.as_array())
            .into_iter()
            .flatten()
            .filter_map(|s| s.as_str().map(str::to_string))
            .collect(),
    })
}

/// What the screen is fed from the server, and where its answers go.
pub(crate) struct Feeds {
    pub(crate) events: mpsc::Sender<AgentEvent>,
    pub(crate) approvals: mpsc::Sender<ApprovalPrompt>,
    pub(crate) questions: mpsc::Sender<crate::mcp::host::Question>,
    pub(crate) tasks: Arc<crate::tools::plan::TaskList>,
}

/// Follow the session `wanted`, the latest one open, or a new one.
/// What it said so far is replayed.
/// Then what it says from now on is fed to the screen until the stream ends.
pub(crate) async fn follow(
    remote: Arc<Remote>,
    hello: &Hello,
    wanted: Option<String>,
    feeds: Feeds,
) -> Result<()> {
    let resp = remote
        .http
        .get(format!("{}/events", remote.base))
        .header("Cookie", &remote.cookie)
        .send()
        .await
        .context("reaching lokode serve")?;
    if !resp.status().is_success() {
        anyhow::bail!("lokode serve answered {}", resp.status());
    }
    let mut events = eventsource_stream::Eventsource::eventsource(resp.bytes_stream());

    let initialized = {
        let remote = remote.clone();
        tokio::spawn(async move {
            remote
                .request(
                    "initialize",
                    json!({
                        "protocolVersion": 1,
                        "clientCapabilities": {"elicitation": {"form": {}}},
                        "clientInfo": {"name": "lokode-attach", "version": env!("CARGO_PKG_VERSION")},
                    }),
                )
                .await
        })
    };
    // The stream is read while the session is chosen, so its answers arrive.
    let (ready_tx, ready_rx) = oneshot::channel::<Vec<Value>>();
    let chosen = {
        let remote = remote.clone();
        let root = hello.root.clone();
        let open = hello.open.clone();
        tokio::spawn(async move {
            let _ = initialized.await;
            let id = match wanted.or_else(|| open.last().cloned()) {
                Some(id) => {
                    remote
                        .request(
                            "session/load",
                            json!({"sessionId": id, "cwd": root, "mcpServers": []}),
                        )
                        .await?;
                    id
                }
                None => remote
                    .request("session/new", json!({"cwd": root, "mcpServers": []}))
                    .await?
                    .get("sessionId")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .to_string(),
            };
            let _ = remote.session.set(id.clone());
            let past = remote
                .own("_lokode/attach", json!({"sessionId": id}))
                .await
                .unwrap_or_default();
            let mut replay: Vec<Value> = past
                .get("updates")
                .and_then(|u| u.as_array())
                .cloned()
                .unwrap_or_default();
            replay.extend(
                past.get("asks")
                    .and_then(|a| a.as_array())
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|ask| json!({"_ask": ask})),
            );
            let _ = ready_tx.send(replay);
            anyhow::Ok(())
        })
    };

    let mut translator = Translator::default();
    let mut ready = Some(ready_rx);
    let mut held: Vec<Value> = Vec::new();
    loop {
        // What the session said before this screen came, once it is known.
        if let Some(rx) = ready.as_mut() {
            if let Ok(replay) = rx.try_recv() {
                ready = None;
                for u in replay {
                    match u.get("_ask") {
                        Some(ask) => asked(&remote, ask.clone(), &feeds).await,
                        None => feed(&mut translator, &u, &feeds).await,
                    }
                }
                for msg in std::mem::take(&mut held) {
                    take(&remote, &mut translator, msg, &feeds).await;
                }
            }
        }
        let next = tokio::select! {
            next = events.next() => next,
            _ = tokio::time::sleep(Duration::from_millis(100)), if ready.is_some() => continue,
        };
        let Some(Ok(event)) = next else { break };
        let Ok(msg) = serde_json::from_str::<Value>(&event.data) else {
            continue;
        };
        // An answer to this screen is taken at once: choosing the session waits on it.
        if msg.get("method").is_none() {
            if let Some(id) = msg.get("id").and_then(|i| i.as_str()) {
                if let Some(tx) = remote.waiting.lock().await.remove(id) {
                    let _ = tx.send(msg);
                    continue;
                }
            }
        }
        match ready.is_some() {
            true => held.push(msg),
            false => take(&remote, &mut translator, msg, &feeds).await,
        }
    }
    chosen.abort();
    Ok(())
}

/// One message of the stream, for the session followed.
async fn take(remote: &Arc<Remote>, translator: &mut Translator, msg: Value, feeds: &Feeds) {
    let ours = msg.pointer("/params/sessionId").and_then(|s| s.as_str())
        == Some(remote.session_id().as_str());
    match (msg.get("method").and_then(|m| m.as_str()), msg.get("id")) {
        (Some("session/update"), None) if ours => {
            let update = msg
                .pointer("/params/update")
                .cloned()
                .unwrap_or(Value::Null);
            // The prompt this screen sent comes back as the server keeps it: drawn once.
            if update.get("sessionUpdate").and_then(|u| u.as_str()) == Some("user_message_chunk") {
                let text = update
                    .pointer("/content/text")
                    .and_then(|t| t.as_str())
                    .unwrap_or_default();
                let mine = remote
                    .sent
                    .lock()
                    .ok()
                    .and_then(|mut s| (s.as_deref() == Some(text)).then(|| s.take()));
                if mine.is_some() {
                    return;
                }
            }
            feed(translator, &update, feeds).await;
        }
        (Some(_), Some(_)) if ours => asked(remote, msg, feeds).await,
        _ => {}
    }
}

async fn feed(translator: &mut Translator, update: &Value, feeds: &Feeds) {
    for event in translator.apply(update) {
        match event {
            Fed::Event(e) => {
                let _ = feeds.events.send(e).await;
            }
            Fed::Plan(tasks) => feeds.tasks.set(tasks).await,
        }
    }
}

/// A question of the server's: an approval or a form, put to the screen and answered.
async fn asked(remote: &Arc<Remote>, msg: Value, feeds: &Feeds) {
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    match msg.get("method").and_then(|m| m.as_str()) {
        Some("session/request_permission") => {
            let option = |kind: &str| {
                params
                    .get("options")
                    .and_then(|o| o.as_array())
                    .into_iter()
                    .flatten()
                    .find(|o| o.get("kind").and_then(|k| k.as_str()) == Some(kind))
                    .cloned()
            };
            let text =
                |v: Option<&Value>| v.and_then(|t| t.as_str()).unwrap_or_default().to_string();
            let call = params.get("toolCall").cloned().unwrap_or(Value::Null);
            let preview: Vec<String> = call
                .get("content")
                .and_then(|c| c.as_array())
                .into_iter()
                .flatten()
                .filter_map(|c| c.pointer("/content/text").and_then(|t| t.as_str()))
                .map(|t| {
                    t.trim_start_matches("```diff\n")
                        .trim_end_matches("```")
                        .trim_end()
                        .to_string()
                })
                .collect();
            let always = option("allow_always");
            let title = text(call.get("title"));
            let req = ApprovalRequest {
                tool: title
                    .split_whitespace()
                    .next()
                    .unwrap_or("call")
                    .to_string(),
                summary: title,
                diff_preview: (!preview.is_empty()).then(|| preview.join("\n")),
                scope: ApprovalScope::Told(
                    always
                        .as_ref()
                        .map(|o| {
                            let name = text(o.get("name"));
                            name.strip_prefix("Always: ").unwrap_or(&name).to_string()
                        })
                        .unwrap_or_else(|| "this call".into()),
                ),
            };
            let (tx, rx) = oneshot::channel();
            let prompt = ApprovalPrompt {
                req,
                reply: tx,
                call: Some(text(call.get("toolCallId"))),
            };
            if feeds.approvals.send(prompt).await.is_err() {
                return;
            }
            let (once, reject) = (option("allow_once"), option("reject_once"));
            let remote = remote.clone();
            tokio::spawn(async move {
                let chosen = match rx.await {
                    Ok(ApprovalDecision::Once) => once,
                    Ok(ApprovalDecision::Always) => always.or(once),
                    _ => reject,
                };
                let id_of = |o: Option<Value>| o.and_then(|o| o.get("optionId").cloned());
                let result = match id_of(chosen) {
                    Some(option) => json!({"outcome": {"outcome": "selected", "optionId": option}}),
                    None => json!({"outcome": {"outcome": "cancelled"}}),
                };
                remote.answer(&id, result).await;
            });
        }
        Some("elicitation/create") => {
            let fields =
                crate::mcp::host::fields(params.get("requestedSchema").unwrap_or(&Value::Null))
                    .unwrap_or_default();
            let (tx, rx) = oneshot::channel();
            let question = crate::mcp::host::Question {
                server: "the session".into(),
                message: params
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or_default()
                    .to_string(),
                fields,
                reply: tx,
            };
            if feeds.questions.send(question).await.is_err() {
                return;
            }
            let remote = remote.clone();
            tokio::spawn(async move {
                let result = match rx.await {
                    Ok(crate::mcp::host::Answer::Accept(content)) => {
                        json!({"action": "accept", "content": content})
                    }
                    _ => json!({"action": "decline"}),
                };
                remote.answer(&id, result).await;
            });
        }
        _ => remote.answer(&id, Value::Null).await,
    }
}

/// What an update gives the screen.
#[derive(Debug)]
enum Fed {
    Event(AgentEvent),
    Plan(Vec<crate::tools::plan::Task>),
}

/// The session's updates, as the engine's events.
#[derive(Default)]
struct Translator {
    /// The answer streamed so far, settled when a step or the turn ends.
    answer: String,
    /// The tools of the calls running, by id.
    calls: HashMap<String, String>,
}

impl Translator {
    fn settle(&mut self) -> Option<Fed> {
        let answer = std::mem::take(&mut self.answer);
        (!answer.is_empty()).then(|| Fed::Event(AgentEvent::AssistantText(answer)))
    }

    fn apply(&mut self, u: &Value) -> Vec<Fed> {
        let text = |p: &str| {
            u.pointer(p)
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_string()
        };
        let mut out = Vec::new();
        match u.get("sessionUpdate").and_then(|s| s.as_str()) {
            Some("agent_message_chunk") => {
                let t = text("/content/text");
                self.answer.push_str(&t);
                out.push(Fed::Event(AgentEvent::AssistantDelta(t)));
            }
            Some("agent_thought_chunk") => {
                out.push(Fed::Event(AgentEvent::Reasoning(text("/content/text"))))
            }
            Some("user_message_chunk") => {
                out.extend(self.settle());
                out.push(Fed::Event(AgentEvent::Asked(text("/content/text"))));
            }
            Some("tool_call") => {
                out.extend(self.settle());
                let id = text("/toolCallId");
                let title = text("/title");
                let name = u
                    .get("name")
                    .and_then(|n| n.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        title
                            .split_whitespace()
                            .next()
                            .unwrap_or("call")
                            .to_string()
                    });
                self.calls.insert(id.clone(), name.clone());
                let args = u.get("rawInput").cloned().unwrap_or(Value::Null);
                out.push(Fed::Event(AgentEvent::ToolCall {
                    id,
                    name,
                    args_pretty: serde_json::to_string_pretty(&args).unwrap_or_default(),
                }));
            }
            Some("tool_call_update") => {
                let status = text("/status");
                if status == "completed" || status == "failed" {
                    let id = text("/toolCallId");
                    let name = self.calls.remove(&id).unwrap_or_default();
                    let content: Vec<String> = u
                        .get("content")
                        .and_then(|c| c.as_array())
                        .into_iter()
                        .flatten()
                        .filter_map(|c| c.pointer("/content/text").and_then(|t| t.as_str()))
                        .map(str::to_string)
                        .collect();
                    let output = match status.as_str() {
                        "failed" => ToolOutput::err(content.join("\n")),
                        _ => ToolOutput::ok(content.join("\n")),
                    };
                    out.push(Fed::Event(AgentEvent::ToolResult {
                        id,
                        name,
                        output,
                        took: Duration::ZERO,
                        origin: Origin::Ran,
                    }));
                }
            }
            Some("plan") => {
                let tasks = u
                    .get("entries")
                    .and_then(|e| e.as_array())
                    .into_iter()
                    .flatten()
                    .map(|e| crate::tools::plan::Task {
                        what: e
                            .get("content")
                            .and_then(|c| c.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        state: match e.get("status").and_then(|s| s.as_str()) {
                            Some("completed") => crate::tools::plan::State::Done,
                            Some("in_progress") => crate::tools::plan::State::Doing,
                            _ => crate::tools::plan::State::Todo,
                        },
                    })
                    .collect();
                out.push(Fed::Plan(tasks));
            }
            Some("current_mode_update") => {
                let mode = match text("/currentModeId").as_str() {
                    "plan" => Mode::Plan,
                    _ => Mode::Act,
                };
                out.push(Fed::Event(AgentEvent::Mode(mode)));
            }
            Some("_lokode/turn") => {
                out.extend(self.settle());
                let meta = u.pointer("/_meta/lokode").cloned().unwrap_or(Value::Null);
                if let Some(w) = meta.get("window") {
                    let n = |k: &str| w.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
                    out.push(Fed::Event(AgentEvent::Window(
                        crate::agent::session::Occupancy {
                            percent: n("percent").min(100) as u8,
                            used: n("used") as usize,
                            budget: n("budget") as usize,
                            parts: Vec::new(),
                        },
                    )));
                }
                if let Some(t) = meta.get("turn") {
                    let n = |k: &str| t.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
                    out.push(Fed::Event(AgentEvent::Receipt(Receipt {
                        turn: 0,
                        took: Duration::from_millis(n("tookMs")),
                        steps: n("steps") as u32,
                        tokens_in: n("tokensIn"),
                        tokens_out: n("tokensOut"),
                        files: t
                            .get("files")
                            .and_then(|f| f.as_array())
                            .into_iter()
                            .flatten()
                            .filter_map(|f| f.as_str().map(str::to_string))
                            .collect(),
                        energy: None,
                    })));
                }
                out.push(Fed::Event(AgentEvent::TurnDone));
            }
            _ => {}
        }
        out
    }
}

/// Send what the screen asks to run to the server, and stop it there when told.
pub(crate) async fn send_turns(
    remote: Arc<Remote>,
    mut turns: mpsc::Receiver<(String, tokio_util::sync::CancellationToken)>,
) {
    while let Some((text, cancel)) = turns.recv().await {
        if let Ok(mut sent) = remote.sent.lock() {
            *sent = Some(text.clone());
        }
        let session = remote.session_id();
        let remote = remote.clone();
        tokio::spawn(async move {
            let prompt = remote.request(
                "session/prompt",
                json!({"sessionId": session, "prompt": [{"type": "text", "text": text}]}),
            );
            tokio::select! {
                _ = prompt => {}
                _ = cancel.cancelled() => {
                    remote.notify("session/cancel", json!({"sessionId": session})).await;
                }
            }
        });
    }
}

/// Answer what the screen asks of the engine.
/// A mode is switched on the server, and the status is given as far as it is known here.
/// Nothing else is answered, since the rest is asked with a command.
pub(crate) async fn answer_controls(
    remote: Arc<Remote>,
    mut controls: mpsc::Receiver<Control>,
    model: String,
) {
    while let Some(control) = controls.recv().await {
        match control {
            Control::Status { reply } => {
                let _ = reply.send(crate::agent::agent_loop::StatusReport {
                    model: model.clone(),
                    window: Default::default(),
                    protocol: crate::agent::protocol::ToolProtocol::Native,
                });
            }
            Control::Mode { mode, reply } => {
                let id = match mode {
                    Mode::Plan => "plan",
                    Mode::Act => "act",
                };
                let said = remote
                    .request(
                        "session/set_mode",
                        json!({"sessionId": remote.session_id(), "modeId": id}),
                    )
                    .await;
                let _ = reply.send(match (said, mode) {
                    (Err(e), _) => format!("the mode did not change: {e:#}"),
                    (Ok(_), Mode::Plan) => "planning: nothing will be changed until /act".into(),
                    (Ok(_), Mode::Act) => {
                        "acting: the agent may change things again, asking as it goes".into()
                    }
                });
            }
            // A request with a reply nobody gets here is answered by dropping it.
            // The screen reads that as nothing said.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A turn's updates become the events a local turn sends, the answer settled once.
    #[test]
    fn updates_become_events() {
        let mut t = Translator::default();
        let mut names = Vec::new();
        for u in [
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Hel"}}),
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "lo"}}),
            json!({"sessionUpdate": "tool_call", "toolCallId": "c1", "title": "write_file a.txt", "kind": "edit", "rawInput": {"path": "a.txt"}}),
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": "completed", "content": [{"type": "content", "content": {"type": "text", "text": "wrote"}}]}),
            json!({"sessionUpdate": "plan", "entries": [{"content": "fix", "priority": "medium", "status": "in_progress"}]}),
            json!({"sessionUpdate": "_lokode/turn", "stopReason": "end_turn", "_meta": {"lokode": {"turn": {"tookMs": 1200, "steps": 2, "tokensIn": 0, "tokensOut": 0, "files": ["a.txt"]}}}}),
        ] {
            for fed in t.apply(&u) {
                names.push(match fed {
                    Fed::Event(AgentEvent::AssistantDelta(s)) => format!("delta {s}"),
                    Fed::Event(AgentEvent::AssistantText(s)) => format!("text {s}"),
                    Fed::Event(AgentEvent::ToolCall { name, .. }) => format!("call {name}"),
                    Fed::Event(AgentEvent::ToolResult { name, output, .. }) => {
                        format!("result {name} {}", output.content)
                    }
                    Fed::Event(AgentEvent::Receipt(r)) => format!("receipt {}", r.files.join(",")),
                    Fed::Event(AgentEvent::TurnDone) => "done".into(),
                    Fed::Plan(tasks) => format!("plan {}", tasks.len()),
                    Fed::Event(other) => format!("{other:?}"),
                });
            }
        }
        assert_eq!(
            names,
            [
                "delta Hel",
                "delta lo",
                "text Hello",
                "call write_file",
                "result write_file wrote",
                "plan 1",
                "receipt a.txt",
                "done"
            ]
        );
    }

    #[test]
    fn the_address_gives_the_server_and_its_token() {
        let r = Remote::new("http://127.0.0.1:8420/?token=abc").unwrap();
        assert_eq!(r.base, "http://127.0.0.1:8420");
        assert_eq!(r.cookie, "lokode-token=abc");
        assert!(Remote::new("http://127.0.0.1:8420/").is_err());
    }
}
