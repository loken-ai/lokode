//! A local address that stands for a session of a remote server.
//!
//! `lokode attach` speaks to the page protocol of `lokode serve` over HTTP.
//! The bridge listens on the loopback address, as a server does, and carries what it is asked
//! over the encrypted channel of the dashboard.
//! The screen that attaches does not know the server is on another machine.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

use super::{read_request, respond, HEARTBEAT};
use crate::dashboard::client::Remote;

/// Listen on the loopback address for the screen that attaches.
///
/// Returns the address to give it. The tasks end when the process does or when `stop` is
/// cancelled.
pub async fn start(
    remote: Arc<Remote>,
    stop: tokio_util::sync::CancellationToken,
) -> Result<String> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let token = crate::mcp::oauth::random();
    let url = format!("http://127.0.0.1:{port}/?token={token}");
    tokio::spawn(async move {
        loop {
            let accepted = tokio::select! {
                accepted = listener.accept() => accepted,
                _ = stop.cancelled() => return,
            };
            let Ok((stream, _)) = accepted else { continue };
            let (remote, token) = (remote.clone(), token.clone());
            tokio::spawn(async move {
                if let Err(e) = handle(&remote, &token, stream).await {
                    tracing::debug!(why = %format!("{e:#}"), "a request to the bridge failed");
                }
            });
        }
    });
    Ok(url)
}

async fn handle(remote: &Remote, token: &str, mut stream: TcpStream) -> Result<()> {
    let Some(req) = read_request(&mut stream).await? else {
        return Ok(());
    };
    let carries = req.carries(token);
    if !carries {
        return respond(&mut stream, "403 Forbidden", "text/plain", &[], b"no token").await;
    }
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/events") => events(remote, stream).await,
        ("POST", "/rpc") => {
            let rpc: Value = serde_json::from_slice(&req.body)?;
            let answer = remote.call(rpc).await?;
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

/// What the server sends, as a page's stream carries it.
async fn events(remote: &Remote, mut stream: TcpStream) -> Result<()> {
    let mut rx = remote.events();
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\n\
              Connection: keep-alive\r\n\r\n: ready\n\n",
        )
        .await?;
    loop {
        let line = tokio::select! {
            got = rx.recv() => match got {
                Ok(msg) if msg["method"] == "_lokode/lagged" => "event: lagged\ndata: {}\n\n".to_string(),
                Ok(msg) => format!("data: {msg}\n\n"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::wire::Group;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn a_screen_that_attaches_reaches_the_remote_server_through_the_bridge() {
        let group = Group {
            name: "home".into(),
            secret: "s3cret".into(),
        };
        let (pages, _) = tokio::sync::broadcast::channel(16);
        let web = Arc::new(super::super::Web {
            token: "t".into(),
            port: 0,
            root: "/remote/project".into(),
            conn: Default::default(),
            pages,
            log: Default::default(),
            asks: Default::default(),
            prompts: Default::default(),
        });
        let addr = super::super::share::start(web, group.clone(), "127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let remote = Arc::new(Remote::open(addr, &group).await.unwrap());
        let stop = tokio_util::sync::CancellationToken::new();
        let url = start(remote, stop.clone()).await.unwrap();

        // The screen's own client for a server, pointed at the bridge.
        let screen = crate::ui::remote::Remote::new(&url).unwrap();
        let hello = crate::ui::remote::hello(&screen).await.unwrap();
        assert_eq!(hello.root.to_string_lossy(), "/remote/project");

        // Without the token the bridge says no.
        let host = url.split('?').next().unwrap().trim_start_matches("http://");
        let mut raw = TcpStream::connect(host.trim_end_matches('/'))
            .await
            .unwrap();
        raw.write_all(b"POST /rpc HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}")
            .await
            .unwrap();
        let mut said = String::new();
        raw.read_to_string(&mut said).await.unwrap();
        assert!(said.contains("403"), "{said}");
        let _ = json!(null);
        stop.cancel();
    }
}
