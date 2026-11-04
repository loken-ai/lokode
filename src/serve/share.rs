//! Sharing the sessions of this server with the dashboards of a group.
//!
//! The server listens on the network, announces itself to the group, and speaks to a dashboard
//! over the encrypted channel of `dashboard::wire`.
//! What a dashboard sends is what a page posts, and what it receives is what a page's stream
//! carries, so a dashboard sees and does what a page does.
//!
//! | From the dashboard | Answered with |
//! |---|---|
//! | `{"tag": n, "rpc": message}` | `{"tag": n, "answer": value}` |
//!
//! | From the server | |
//! |---|---|
//! | `{"event": message}` | what lokode sent, as a page's stream carries it |
//! | `{"lagged": true}` | the dashboard fell behind, and attaches again |

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};

use super::{posted, Web, HEARTBEAT};
use crate::dashboard::discovery::{announcing, Announcement};
use crate::dashboard::wire::{accept, Group};

/// How long a peer has to finish the handshake.
const HANDSHAKE: Duration = Duration::from_secs(10);

/// Listen for the dashboards of `group` and announce this server to it.
///
/// It listens on `listen`, and returns the address it listens on.
/// The tasks end with the process.
pub(super) async fn start(
    web: Arc<Web>,
    group: Group,
    listen: std::net::SocketAddr,
) -> Result<std::net::SocketAddr> {
    let listener = TcpListener::bind(listen).await?;
    let bound = listener.local_addr()?;
    let port = bound.port();
    // A server on the loopback address cannot be reached from the machines that would hear it.
    if !bound.ip().is_loopback() {
        announcing(
            Announcement {
                group: group.name.clone(),
                node: crate::mcp::oauth::random().chars().take(12).collect(),
                port,
            },
            group.clone(),
        );
    }
    tokio::spawn(async move {
        loop {
            let Ok((stream, from)) = listener.accept().await else {
                continue;
            };
            let (web, group) = (web.clone(), group.clone());
            tokio::spawn(async move {
                if let Err(e) = session(&web, stream, &group).await {
                    tracing::debug!(%from, why = %format!("{e:#}"), "a dashboard left");
                }
            });
        }
    });
    Ok(bound)
}

/// One dashboard, until it goes.
async fn session(web: &Arc<Web>, stream: TcpStream, group: &Group) -> Result<()> {
    let (mut send, mut recv) = tokio::time::timeout(HANDSHAKE, accept(stream, group)).await??;
    let mut events = web.pages.subscribe();
    let (out, mut queued) = tokio::sync::mpsc::unbounded_channel::<Value>();

    let answers = out.clone();
    let web_for_reader = web.clone();
    let reader = tokio::spawn(async move {
        while let Ok(Some(message)) = recv.recv().await {
            let Some(rpc) = message.get("rpc").cloned() else {
                continue;
            };
            let tag = message.get("tag").cloned().unwrap_or(Value::Null);
            let answer = posted(&web_for_reader, rpc).await;
            let _ = answers.send(json!({"tag": tag, "answer": answer}));
        }
    });

    let result = loop {
        let message = tokio::select! {
            queued = queued.recv() => match queued {
                Some(message) => message,
                None => break Ok(()),
            },
            got = events.recv() => match got {
                Ok(text) => json!({"event": serde_json::from_str::<Value>(&text).unwrap_or(Value::Null)}),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => json!({"lagged": true}),
                Err(_) => break Ok(()),
            },
            _ = tokio::time::sleep(HEARTBEAT) => json!({"ping": true}),
            _ = async { while !reader.is_finished() { tokio::time::sleep(Duration::from_millis(200)).await } } => break Ok(()),
        };
        if let Err(e) = send.send(&message).await {
            break Err(e.into());
        }
    };
    reader.abort();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::wire::connect;

    fn web() -> Arc<Web> {
        let (pages, _) = tokio::sync::broadcast::channel(16);
        Arc::new(Web {
            token: "t".into(),
            port: 0,
            root: "/p".into(),
            conn: Default::default(),
            pages,
            log: Default::default(),
            asks: Default::default(),
            prompts: Default::default(),
        })
    }

    fn group(secret: &str) -> Group {
        Group {
            name: "home".into(),
            secret: secret.into(),
        }
    }

    /// A dashboard of the group asks the server who it is, and hears what the server sends.
    #[tokio::test]
    async fn a_dashboard_of_the_group_is_answered_and_hears_the_events() {
        let web = web();
        let port = start(web.clone(), group("s3cret"), "127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let stream = TcpStream::connect(port).await.unwrap();
        let (mut send, mut recv) = connect(stream, &group("s3cret")).await.unwrap();
        send.send(&json!({"tag": 7, "rpc": {"method": "_lokode/hello"}}))
            .await
            .unwrap();
        let answer = loop {
            let message = recv.recv().await.unwrap().unwrap();
            if message.get("tag") == Some(&json!(7)) {
                break message;
            }
        };
        assert_eq!(answer["answer"]["result"]["root"], "/p");

        web.send(
            json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s"}}),
        );
        let heard = loop {
            let message = recv.recv().await.unwrap().unwrap();
            if message.get("event").is_some() {
                break message;
            }
        };
        assert_eq!(heard["event"]["params"]["sessionId"], "s");
    }

    /// A machine without the secret is dropped before it can ask for anything.
    #[tokio::test]
    async fn a_machine_without_the_secret_is_dropped() {
        let port = start(web(), group("s3cret"), "127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let stream = TcpStream::connect(port).await.unwrap();
        let got = connect(stream, &group("wrong")).await;
        match got {
            Err(_) => {}
            Ok((_, mut recv)) => assert!(recv.recv().await.is_err()),
        }
    }
}
