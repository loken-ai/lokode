//! `lokode serve`: the sessions of a project in a browser page, on this machine only.
//!
//! The page speaks the Agent Client Protocol, as an editor does.
//! What lokode sends reaches every open page as a stream of events.
//! What a page sends is posted.
//! 2 requests of lokode's own complete the protocol for pages that come and go:
//!
//! | Request | Answer |
//! |---|---|
//! | `_lokode/hello` | the project, the sessions open and those recorded |
//! | `_lokode/attach` | everything a session said so far, and what it still waits on |
//!
//! The end of a turn reaches every page as a session update of lokode's own, `_lokode/turn`.
//! It carries why the turn stopped and what it came to.
//!
//! | Guard | What it keeps out |
//! |---|---|
//! | the loopback address only | other machines |
//! | a token in the printed address, then a cookie | other users of this machine |
//! | `Host` and `Origin` checked | a page elsewhere reaching it through DNS rebinding |

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::acp::{self, ConfigFor, Conn};

const PAGE: &str = include_str!("index.html");
const SCRIPT: &str = include_str!("page.js");
const STYLE: &str = include_str!("page.css");

/// Bytes a request's head and body may take.
const MAX_REQUEST: usize = 4 * 1024 * 1024;

/// How often an idle stream of events says it is still there.
const HEARTBEAT: Duration = Duration::from_secs(15);

/// Messages kept for a page that falls behind the stream.
const BACKLOG: usize = 1024;

/// Updates kept per session for a page that attaches later.
const KEPT_UPDATES: usize = 20_000;

/// Recorded sessions a page lists, the most recent first.
const RECORDED_SHOWN: usize = 20;

/// The cookie the token is kept in.
const COOKIE: &str = "lokode-token";

/// What the server shares between its pages.
struct Web {
    token: String,
    port: u16,
    root: PathBuf,
    conn: OnceLock<Arc<Conn>>,
    pages: tokio::sync::broadcast::Sender<String>,
    /// What each session said, for a page that attaches later.
    log: std::sync::Mutex<HashMap<String, Vec<Value>>>,
    /// What lokode asked and nobody answered yet, by id.
    asks: std::sync::Mutex<BTreeMap<i64, Value>>,
    /// The prompts running, by the id a page gave them, with their session.
    prompts: std::sync::Mutex<HashMap<String, String>>,
}

impl Web {
    /// What lokode sends.
    /// It is kept where a later page needs it, and given to every page.
    fn send(&self, msg: Value) {
        let method = msg.get("method").and_then(|m| m.as_str());
        if method == Some("session/update") {
            let session = msg
                .pointer("/params/sessionId")
                .and_then(|s| s.as_str())
                .unwrap_or_default()
                .to_string();
            if let Ok(mut log) = self.log.lock() {
                let kept = log.entry(session).or_default();
                if kept.len() < KEPT_UPDATES {
                    kept.push(
                        msg.pointer("/params/update")
                            .cloned()
                            .unwrap_or(Value::Null),
                    );
                }
            }
        }
        if let (Some(id), Some(_)) = (msg.get("id").and_then(|i| i.as_i64()), method) {
            if let Ok(mut asks) = self.asks.lock() {
                asks.insert(id, msg.clone());
            }
        }
        // The answer to a prompt: the end of a turn, for every page and for later ones.
        let ended = match (msg.get("id"), method, msg.get("result")) {
            (Some(id), None, Some(result)) => self
                .prompts
                .lock()
                .ok()
                .and_then(|mut p| p.remove(&id.to_string()))
                .map(|session| (session, result.clone())),
            _ => None,
        };
        let _ = self.pages.send(msg.to_string());
        if let Some((session, result)) = ended {
            self.send(
                json!({"jsonrpc": "2.0", "method": "session/update", "params": {
                    "sessionId": session,
                    "update": {
                        "sessionUpdate": "_lokode/turn",
                        "stopReason": result.get("stopReason"),
                        "_meta": result.get("_meta"),
                    },
                }}),
            );
        }
    }

    fn conn(&self) -> &Arc<Conn> {
        self.conn
            .get()
            .expect("the connection is made before serving")
    }

    /// Whether `host` names this server, as the page's own address does.
    fn ours(&self, host: &str) -> bool {
        host == format!("127.0.0.1:{}", self.port) || host == format!("localhost:{}", self.port)
    }
}

/// Serve the project at `root` on `port` of the loopback address, 0 for any, until stopped.
pub async fn run(config_for: ConfigFor, root: PathBuf, port: u16) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let port = listener.local_addr()?.port();
    let (pages, _) = tokio::sync::broadcast::channel(BACKLOG);
    let web = Arc::new(Web {
        token: crate::mcp::oauth::random(),
        port,
        root,
        conn: OnceLock::new(),
        pages,
        log: Default::default(),
        asks: Default::default(),
        prompts: Default::default(),
    });
    let sink = web.clone();
    let _ = web.conn.set(acp::connect(
        Box::new(move |msg| sink.send(msg)),
        config_for,
    ));
    println!(
        "lokode serves {} at http://127.0.0.1:{port}/?token={}",
        web.root.display(),
        web.token
    );
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                let web = web.clone();
                tokio::spawn(async move {
                    if let Err(e) = serve(&web, stream).await {
                        tracing::debug!(why = %format!("{e:#}"), "a page's request failed");
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    acp::close(web.conn()).await;
    Ok(())
}

/// One request read off a connection.
struct Request {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    /// Whether it carries the token, in its cookie or in its address.
    fn carries(&self, token: &str) -> bool {
        let in_cookie = self.header("cookie").is_some_and(|c| {
            c.split(';')
                .filter_map(|kv| kv.trim().split_once('='))
                .any(|(k, v)| k == COOKIE && v == token)
        });
        in_cookie || self.query.get("token").map(String::as_str) == Some(token)
    }
}

async fn read_request(stream: &mut TcpStream) -> Result<Option<Request>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break at;
        }
        if buf.len() > MAX_REQUEST {
            anyhow::bail!("a request head larger than allowed");
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.lines();
    let first = lines.next().unwrap_or_default();
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let headers: HashMap<String, String> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_string()))
        .collect();
    let length: usize = headers
        .get("content-length")
        .and_then(|l| l.parse().ok())
        .unwrap_or(0);
    if length > MAX_REQUEST {
        anyhow::bail!("a request body larger than allowed");
    }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < length {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(length);
    let url = url::Url::parse(&format!("http://local{target}"))?;
    Ok(Some(Request {
        method,
        path: url.path().to_string(),
        query: url.query_pairs().into_owned().collect(),
        headers,
        body,
    }))
}

async fn respond(
    stream: &mut TcpStream,
    status: &str,
    kind: &str,
    extra: &[(&str, String)],
    body: &[u8],
) -> Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'self'; frame-ancestors 'none'\r\n\
         Connection: close\r\n",
        body.len()
    );
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.shutdown().await.ok();
    Ok(())
}

async fn serve(web: &Arc<Web>, mut stream: TcpStream) -> Result<()> {
    let Some(req) = read_request(&mut stream).await? else {
        return Ok(());
    };
    if !req.header("host").is_some_and(|h| web.ours(h)) {
        return respond(
            &mut stream,
            "403 Forbidden",
            "text/plain",
            &[],
            b"wrong host",
        )
        .await;
    }
    if let Some(origin) = req.header("origin") {
        if !origin.strip_prefix("http://").is_some_and(|h| web.ours(h)) {
            return respond(
                &mut stream,
                "403 Forbidden",
                "text/plain",
                &[],
                b"wrong origin",
            )
            .await;
        }
    }
    if !req.carries(&web.token) {
        return respond(
            &mut stream,
            "401 Unauthorized",
            "text/plain; charset=utf-8",
            &[],
            b"Open the address lokode serve printed, with its token.",
        )
        .await;
    }
    // The token leaves the address for a cookie, so it is not kept in the history.
    if req.query.contains_key("token") {
        let cookie = format!("{COOKIE}={}; HttpOnly; SameSite=Strict; Path=/", web.token);
        return respond(
            &mut stream,
            "303 See Other",
            "text/plain",
            &[("Set-Cookie", cookie), ("Location", req.path.clone())],
            b"",
        )
        .await;
    }
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") => {
            respond(
                &mut stream,
                "200 OK",
                "text/html; charset=utf-8",
                &[],
                PAGE.as_bytes(),
            )
            .await
        }
        ("GET", "/page.js") => {
            respond(
                &mut stream,
                "200 OK",
                "text/javascript; charset=utf-8",
                &[],
                SCRIPT.as_bytes(),
            )
            .await
        }
        ("GET", "/page.css") => {
            respond(
                &mut stream,
                "200 OK",
                "text/css; charset=utf-8",
                &[],
                STYLE.as_bytes(),
            )
            .await
        }
        ("GET", "/events") => events(web, stream).await,
        ("POST", "/rpc") => {
            let msg: Value = serde_json::from_slice(&req.body)?;
            let answer = posted(web, msg).await;
            respond(
                &mut stream,
                "200 OK",
                "application/json",
                &[],
                answer.to_string().as_bytes(),
            )
            .await
        }
        _ => {
            respond(
                &mut stream,
                "404 Not Found",
                "text/plain",
                &[],
                b"nothing here",
            )
            .await
        }
    }
}

/// The stream of what lokode sends, to one page, until it goes.
async fn events(web: &Arc<Web>, mut stream: TcpStream) -> Result<()> {
    let mut rx = web.pages.subscribe();
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\n\
              Connection: keep-alive\r\n\r\n: ready\n\n",
        )
        .await?;
    loop {
        let line = tokio::select! {
            got = rx.recv() => match got {
                Ok(msg) => format!("data: {msg}\n\n"),
                // A page that fell behind reloads what it missed by attaching again.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    "event: lagged\ndata: {}\n\n".to_string()
                }
                Err(_) => return Ok(()),
            },
            _ = tokio::time::sleep(HEARTBEAT) => ": ping\n\n".to_string(),
        };
        stream.write_all(line.as_bytes()).await?;
    }
}

/// A message a page posted.
/// It is answered here when it is one of lokode's own.
/// Otherwise it is handed to the session, and its answer comes on the stream.
async fn posted(web: &Arc<Web>, msg: Value) -> Value {
    let method = msg
        .get("method")
        .and_then(|m| m.as_str())
        .unwrap_or_default();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    match method {
        "_lokode/hello" => {
            let open: Vec<String> = web
                .log
                .lock()
                .map(|l| l.keys().cloned().collect())
                .unwrap_or_default();
            let recorded: Vec<Value> = crate::paths::sessions_dir()
                .map(|dir| crate::agent::transcript::recorded_for_root(&dir, &web.root))
                .unwrap_or_default()
                .into_iter()
                .filter(|r| r.messages > 0)
                .take(RECORDED_SHOWN)
                .map(|r| json!({"id": r.id, "when": r.when, "messages": r.messages, "opening": r.opening}))
                .collect();
            json!({"result": {
                "root": web.root,
                "version": env!("CARGO_PKG_VERSION"),
                "open": open,
                "recorded": recorded,
            }})
        }
        "_lokode/attach" => {
            let id = params
                .get("sessionId")
                .and_then(|s| s.as_str())
                .unwrap_or_default();
            let updates = web
                .log
                .lock()
                .ok()
                .and_then(|l| l.get(id).cloned())
                .unwrap_or_default();
            let asks: Vec<Value> = web
                .asks
                .lock()
                .map(|a| {
                    a.values()
                        .filter(|m| {
                            m.pointer("/params/sessionId").and_then(|s| s.as_str()) == Some(id)
                        })
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            json!({"result": {"updates": updates, "asks": asks}})
        }
        "" => {
            // An answer to a question of lokode's: settled for every page.
            if let Some(id) = msg.get("id").and_then(|i| i.as_i64()) {
                let was_asked = web
                    .asks
                    .lock()
                    .map(|mut a| a.remove(&id).is_some())
                    .unwrap_or(false);
                if !was_asked {
                    return json!({"accepted": false});
                }
                let _ = web.pages.send(
                    json!({"jsonrpc": "2.0", "method": "_lokode/settled", "params": {"id": id}})
                        .to_string(),
                );
            }
            acp::dispatch(web.conn(), msg);
            json!({"accepted": true})
        }
        _ => {
            // What was asked is part of the thread every page shows.
            // An editor's agent does not echo it.
            if method == "session/prompt" {
                if let (Some(id), Some(session)) = (
                    msg.get("id"),
                    params.get("sessionId").and_then(|s| s.as_str()),
                ) {
                    if let Ok(mut p) = web.prompts.lock() {
                        p.insert(id.to_string(), session.to_string());
                    }
                }
                let text: Vec<&str> = params
                    .get("prompt")
                    .and_then(|p| p.as_array())
                    .into_iter()
                    .flatten()
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                    .collect();
                web.send(json!({"jsonrpc": "2.0", "method": "session/update", "params": {
                    "sessionId": params.get("sessionId"),
                    "update": {"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": text.join("\n")}},
                }}));
            }
            acp::dispatch(web.conn(), msg);
            json!({"accepted": true})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn web(port: u16) -> Web {
        Web {
            token: "t0k".into(),
            port,
            root: PathBuf::from("/p"),
            conn: OnceLock::new(),
            pages: tokio::sync::broadcast::channel(4).0,
            log: Default::default(),
            asks: Default::default(),
            prompts: Default::default(),
        }
    }

    /// Only this server's own address is taken, and the token is found in either place.
    #[test]
    fn the_guards_hold() {
        let w = web(4242);
        assert!(w.ours("127.0.0.1:4242") && w.ours("localhost:4242"));
        assert!(!w.ours("evil.example:4242") && !w.ours("127.0.0.1:1"));
        let req = |cookie: Option<&str>, query: Option<&str>| Request {
            method: "GET".into(),
            path: "/".into(),
            query: query
                .map(|q| [("token".to_string(), q.to_string())].into())
                .unwrap_or_default(),
            headers: cookie
                .map(|c| [("cookie".to_string(), c.to_string())].into())
                .unwrap_or_default(),
            body: Vec::new(),
        };
        assert!(req(Some("a=b; lokode-token=t0k"), None).carries("t0k"));
        assert!(req(None, Some("t0k")).carries("t0k"));
        assert!(!req(Some("lokode-token=nope"), None).carries("t0k"));
        assert!(!req(None, None).carries("t0k"));
    }

    /// What lokode sends is kept by session, and a question until it is answered.
    #[test]
    fn updates_and_questions_are_kept() {
        let w = web(1);
        w.send(json!({"method": "session/update", "params": {"sessionId": "s", "update": {"sessionUpdate": "plan"}}}));
        w.send(
            json!({"id": 3, "method": "session/request_permission", "params": {"sessionId": "s"}}),
        );
        w.send(json!({"id": 9, "result": {}}));
        assert_eq!(w.log.lock().unwrap()["s"].len(), 1);
        assert_eq!(w.asks.lock().unwrap().len(), 1);
        // The answer to a prompt ends the turn in the session's log.
        w.prompts
            .lock()
            .unwrap()
            .insert(json!("tab-1").to_string(), "s".into());
        w.send(
            json!({"id": "tab-1", "result": {"stopReason": "end_turn", "_meta": {"lokode": {}}}}),
        );
        let log = w.log.lock().unwrap();
        assert_eq!(log["s"][1]["sessionUpdate"], "_lokode/turn");
        assert_eq!(log["s"][1]["stopReason"], "end_turn");
    }
}
