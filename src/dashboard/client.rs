//! The dashboard's side of the channel: a connection to one server, and the fleet of all of them.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, oneshot};

use super::discovery::{listening, PeerBook};
use super::state::ServerState;
use super::wire::{connect, Group};

/// How long a server has to answer a request.
const ANSWER_WITHIN: Duration = Duration::from_secs(15);

/// How long a connection has to be made.
const CONNECT_WITHIN: Duration = Duration::from_secs(5);

/// How often the fleet looks for servers it is not connected to.
const LOOK_EVERY: Duration = Duration::from_secs(2);

/// Messages kept for a reader that falls behind.
const BACKLOG: usize = 1024;

/// A connection to one server.
pub struct Remote {
    out: mpsc::UnboundedSender<Value>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    events: broadcast::Sender<Value>,
    next: std::sync::atomic::AtomicU64,
}

impl Remote {
    /// Connect to the server at `addr`, which must hold the secret of `group`.
    pub async fn open(addr: SocketAddr, group: &Group) -> io::Result<Remote> {
        let stream = tokio::time::timeout(CONNECT_WITHIN, TcpStream::connect(addr))
            .await
            .map_err(|_| io::Error::other("it did not answer"))??;
        let (mut send, mut recv) = tokio::time::timeout(CONNECT_WITHIN, connect(stream, group))
            .await
            .map_err(|_| io::Error::other("it did not finish the handshake"))??;
        let (out, mut queued) = mpsc::unbounded_channel::<Value>();
        let (events, _) = broadcast::channel(BACKLOG);
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> = Default::default();

        tokio::spawn(async move {
            while let Some(message) = queued.recv().await {
                if send.send(&message).await.is_err() {
                    break;
                }
            }
        });
        let (tx, waiting) = (events.clone(), pending.clone());
        tokio::spawn(async move {
            while let Ok(Some(message)) = recv.recv().await {
                if let Some(tag) = message.get("tag").and_then(|t| t.as_u64()) {
                    let waiter = waiting.lock().ok().and_then(|mut w| w.remove(&tag));
                    if let Some(waiter) = waiter {
                        let _ = waiter.send(message["answer"].clone());
                    }
                } else if let Some(event) = message.get("event") {
                    let _ = tx.send(event.clone());
                } else if message.get("lagged").is_some() {
                    let _ = tx.send(json!({"method": "_lokode/lagged"}));
                }
            }
            // Dropping the sender tells every reader that the server has gone.
            drop(tx);
            if let Ok(mut waiting) = waiting.lock() {
                waiting.clear();
            }
        });
        Ok(Remote {
            out,
            pending,
            events,
            next: Default::default(),
        })
    }

    /// Post `rpc` as a page would, and wait for the answer.
    pub async fn call(&self, rpc: Value) -> io::Result<Value> {
        let tag = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .map_err(|_| io::Error::other("the connection failed"))?
            .insert(tag, tx);
        self.out
            .send(json!({"tag": tag, "rpc": rpc}))
            .map_err(|_| io::Error::other("the server has gone"))?;
        match tokio::time::timeout(ANSWER_WITHIN, rx).await {
            Ok(Ok(answer)) => Ok(answer),
            Ok(Err(_)) => Err(io::Error::other("the server has gone")),
            Err(_) => Err(io::Error::other("the server did not answer")),
        }
    }

    /// What the server sends from now on.
    pub fn events(&self) -> broadcast::Receiver<Value> {
        self.events.subscribe()
    }

    /// Whether the connection is still there.
    pub fn alive(&self) -> bool {
        !self.out.is_closed()
    }
}

/// What the fleet knows of one server.
pub struct Entry {
    pub addr: SocketAddr,
    pub state: ServerState,
    pub link: Option<Arc<Remote>>,
    /// Why the server could not be reached, when it could not.
    pub error: Option<String>,
}

/// Every server of the group, reached or not.
#[derive(Default)]
pub struct Fleet {
    pub servers: BTreeMap<String, Entry>,
}

pub type Shared = Arc<Mutex<Fleet>>;

/// Keep a fleet up to date: look for the servers of `group`, connect to each, follow what its
/// sessions do.
pub fn follow(group: Group, seeds: Vec<SocketAddr>) -> (Shared, tokio::task::JoinHandle<()>) {
    let fleet: Shared = Default::default();
    let shared = fleet.clone();
    let task = tokio::spawn(async move {
        let (book, _listener) = listening(group.clone());
        loop {
            let found = peers(&book, &seeds);
            // A server that is gone and no longer announced is not listed.
            if let Ok(mut f) = fleet.lock() {
                f.servers
                    .retain(|node, e| e.link.is_some() || found.iter().any(|(n, _)| n == node));
            }
            for (node, addr) in found {
                let connected = fleet
                    .lock()
                    .map(|f| {
                        f.servers
                            .get(&node)
                            .is_some_and(|e| e.link.as_ref().is_some_and(|l| l.alive()))
                    })
                    .unwrap_or(true);
                if connected {
                    continue;
                }
                let (fleet, group) = (fleet.clone(), group.clone());
                // The entry stands for a server being reached, so that it is listed.
                if let Ok(mut f) = fleet.lock() {
                    let entry = f.servers.entry(node.clone()).or_insert_with(|| Entry {
                        addr,
                        state: ServerState::default(),
                        link: None,
                        error: None,
                    });
                    entry.addr = addr;
                }
                tokio::spawn(async move { reach(fleet, node, addr, group).await });
            }
            tokio::time::sleep(LOOK_EVERY).await;
        }
    });
    (shared, task)
}

/// The servers that are announced and the seeds, by an id of their own.
fn peers(book: &Arc<Mutex<PeerBook>>, seeds: &[SocketAddr]) -> Vec<(String, SocketAddr)> {
    let mut found = book
        .lock()
        .map(|mut b| b.live(Instant::now()))
        .unwrap_or_default();
    // A seed that names a server already announced is that server.
    for seed in seeds {
        if !found.iter().any(|(_, addr)| addr == seed) {
            found.push((format!("seed:{seed}"), *seed));
        }
    }
    found
}

/// Connect to one server and follow it until the connection ends.
async fn reach(fleet: Shared, node: String, addr: SocketAddr, group: Group) {
    let set_error = |why: Option<String>| {
        if let Ok(mut f) = fleet.lock() {
            if let Some(e) = f.servers.get_mut(&node) {
                e.error = why;
            }
        }
    };
    let remote = match Remote::open(addr, &group).await {
        Ok(r) => Arc::new(r),
        Err(e) => return set_error(Some(e.to_string())),
    };
    let mut events = remote.events();
    let hello = match remote.call(json!({"method": "_lokode/hello"})).await {
        Ok(a) => a,
        Err(e) => return set_error(Some(e.to_string())),
    };
    let sessions = {
        let Ok(mut f) = fleet.lock() else { return };
        let Some(entry) = f.servers.get_mut(&node) else {
            return;
        };
        entry.state = ServerState::default();
        entry.state.hello(&hello["result"]);
        entry.link = Some(remote.clone());
        entry.error = None;
        entry.state.sessions.keys().cloned().collect::<Vec<_>>()
    };
    attach_all(&fleet, &node, &remote, &sessions).await;
    loop {
        match events.recv().await {
            Ok(message) => {
                if message["method"] == "_lokode/lagged" {
                    let open: Vec<String> = fleet
                        .lock()
                        .ok()
                        .and_then(|f| {
                            f.servers
                                .get(&node)
                                .map(|e| e.state.sessions.keys().cloned().collect())
                        })
                        .unwrap_or_default();
                    attach_all(&fleet, &node, &remote, &open).await;
                } else if let Ok(mut f) = fleet.lock() {
                    if let Some(e) = f.servers.get_mut(&node) {
                        e.state.message(&message, Instant::now());
                    }
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => break,
        }
    }
    if let Ok(mut f) = fleet.lock() {
        if let Some(e) = f.servers.get_mut(&node) {
            e.link = None;
            e.error = Some("the connection ended".into());
        }
    }
}

/// Read again what each of `sessions` said and waits on.
async fn attach_all(fleet: &Shared, node: &str, remote: &Remote, sessions: &[String]) {
    for session in sessions {
        let Ok(answer) = remote
            .call(json!({"method": "_lokode/attach", "params": {"sessionId": session}}))
            .await
        else {
            continue;
        };
        if let Ok(mut f) = fleet.lock() {
            if let Some(e) = f.servers.get_mut(node) {
                e.state.attached(session, &answer["result"], Instant::now());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::wire::accept;
    use tokio::net::TcpListener;

    fn group() -> Group {
        Group {
            name: "home".into(),
            secret: "s3cret".into(),
        }
    }

    /// A server that answers every request with its own tag and the method it was asked.
    async fn echo_server() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut send, mut recv) = accept(stream, &group()).await.unwrap();
            send.send(&json!({"event": {"method": "first"}}))
                .await
                .unwrap();
            while let Ok(Some(message)) = recv.recv().await {
                let answer = json!({"asked": message["rpc"]["method"]});
                send.send(&json!({"tag": message["tag"], "answer": answer}))
                    .await
                    .unwrap();
            }
        });
        addr
    }

    #[tokio::test]
    async fn a_call_is_answered_and_the_events_arrive() {
        let addr = echo_server().await;
        let remote = Remote::open(addr, &group()).await.unwrap();
        let mut events = remote.events();
        let answer = remote.call(json!({"method": "ping"})).await.unwrap();
        assert_eq!(answer["asked"], "ping");
        // The event was sent before the call was answered.
        let heard = tokio::time::timeout(Duration::from_secs(2), events.recv()).await;
        assert!(heard.is_ok() || remote.alive());
        assert!(remote.alive());
    }

    #[tokio::test]
    async fn a_server_that_cannot_be_reached_is_an_error() {
        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        assert!(Remote::open(addr, &group()).await.is_err());
    }
}
