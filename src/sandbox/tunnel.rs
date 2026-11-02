//! The command that carries one TCP stream through the proxy of the session.
//!
//! A client that does not read `https_proxy` can still be given a command to run in front of
//! its connection, as `ssh` is with `ProxyCommand`.
//! `lokode tunnel HOST PORT` is that command.
//! It asks the proxy for a tunnel to the host and copies standard input and output through it.
//!
//! The proxy and the lease come from the environment the command was started with.
//! Outside a confined command there is neither, and the tunnel refuses to start.

use base64::Engine;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

/// The bytes of a proxy answer's head that are read before the tunnel is given up on.
const MAX_HEAD_BYTES: usize = 8 * 1024;

/// Where the proxy is and the credential that names the lease.
#[derive(Debug, PartialEq, Eq)]
pub struct Route {
    addr: String,
    credential: String,
}

impl Route {
    /// The route named by a proxy address of the form `http://TOKEN:x@HOST:PORT`.
    pub fn parse(url: &str) -> Result<Self, String> {
        let rest = url
            .strip_prefix("http://")
            .ok_or("the proxy address is not an http address")?;
        let (userinfo, addr) = rest
            .rsplit_once('@')
            .ok_or("the proxy address carries no lease")?;
        let addr = addr.trim_end_matches('/');
        if userinfo.is_empty() || addr.is_empty() {
            return Err("the proxy address is incomplete".into());
        }
        Ok(Self {
            addr: addr.into(),
            credential: base64::engine::general_purpose::STANDARD.encode(userinfo),
        })
    }

    /// The route of the command this one runs in, read from its environment.
    pub fn from_env() -> Result<Self, String> {
        let url = std::env::var("https_proxy")
            .or_else(|_| std::env::var("HTTPS_PROXY"))
            .map_err(|_| "this command was not started by a lokode session with a proxy")?;
        Self::parse(&url)
    }
}

/// Ask the proxy for a tunnel to `host:port`, then carry `input` and `output` through it.
///
/// A refusal is returned with what the proxy said, so the client that ran this shows it.
pub async fn carry(
    route: &Route,
    host: &str,
    port: u16,
    input: impl AsyncRead + Unpin,
    output: impl AsyncWrite + Unpin,
) -> Result<(), String> {
    let mut stream = TcpStream::connect(&route.addr)
        .await
        .map_err(|e| format!("the proxy at {} cannot be reached: {e}", route.addr))?;
    let target = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let request = format!(
        "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\nProxy-Authorization: Basic {}\r\n\r\n",
        route.credential
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| format!("the proxy did not take the request: {e}"))?;

    // One byte at a time: whatever follows the head belongs to the tunnel.
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= MAX_HEAD_BYTES {
            return Err("the proxy answer is too long".into());
        }
        match stream.read(&mut byte).await {
            Ok(1) => head.push(byte[0]),
            _ => return Err("the proxy closed the connection before it answered".into()),
        }
    }
    let status = String::from_utf8_lossy(&head);
    let status = status.lines().next().unwrap_or_default();
    if status.split_whitespace().nth(1) != Some("200") {
        let mut body = String::new();
        let _ = stream.read_to_string(&mut body).await;
        let said = body.trim();
        return Err(if said.is_empty() {
            status.to_string()
        } else {
            said.to_string()
        });
    }

    let mut local = tokio::io::join(input, output);
    tokio::io::copy_bidirectional(&mut local, &mut stream)
        .await
        .map(|_| ())
        .map_err(|e| format!("the tunnel to {target} broke: {e}"))
}

/// Carry this process's standard input and output to `host:port`.
pub async fn run(host: &str, port: u16) -> Result<(), String> {
    let route = Route::from_env()?;
    carry(&route, host, port, tokio::io::stdin(), tokio::io::stdout()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::approval::{Approval, ApprovalRequest, Denial};
    use crate::sandbox::egress::{Gate, Journal, Proxy};
    use async_trait::async_trait;
    use std::sync::Arc;
    use tokio::net::TcpListener;

    struct Answer(bool);

    #[async_trait]
    impl Gate for Answer {
        async fn approve(&self, _req: ApprovalRequest) -> Approval {
            if self.0 {
                Approval::Proceed
            } else {
                Approval::Denied(Denial::User(None))
            }
        }
    }

    /// A proxy that reaches loopback, and the route of a lease decided by `agree`.
    async fn route_for(agree: bool) -> (Proxy, crate::sandbox::egress::Lease, Route) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let allow = Arc::new(["127.0.0.1".to_string()].into_iter().collect());
        let proxy = Proxy::serve(listener, allow, Arc::new(Journal::default())).unwrap();
        let lease = proxy.lease(Arc::new(Answer(agree)), "ssh");
        let url = lease
            .env()
            .into_iter()
            .find(|(name, _)| name == "https_proxy")
            .unwrap()
            .1;
        let route = Route::parse(&url).unwrap();
        (proxy, lease, route)
    }

    /// A service that sends a greeting and echoes one line back.
    async fn service() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let _ = socket.write_all(b"hello\n").await;
                    let mut buf = [0u8; 64];
                    if let Ok(n) = socket.read(&mut buf).await {
                        let _ = socket.write_all(&buf[..n]).await;
                    }
                });
            }
        });
        port
    }

    #[test]
    fn a_route_comes_from_a_proxy_address_with_a_lease() {
        let route = Route::parse("http://abc:x@127.0.0.1:4000").unwrap();
        assert_eq!(route.addr, "127.0.0.1:4000");
        assert_eq!(
            route.credential,
            base64::engine::general_purpose::STANDARD.encode("abc:x")
        );
    }

    #[test]
    fn an_address_with_no_lease_is_not_a_route() {
        assert!(Route::parse("http://127.0.0.1:4000").is_err());
        assert!(Route::parse("socks5h://abc:x@127.0.0.1:4000").is_err());
        assert!(Route::parse("http://@127.0.0.1:4000").is_err());
    }

    /// The bytes the service sends reach the output, and the bytes of the input reach the service.
    #[tokio::test]
    async fn a_stream_is_carried_both_ways_to_an_agreed_host() {
        let (_proxy, _lease, route) = route_for(true).await;
        let port = service().await;
        let (mut input_tx, input) = tokio::io::duplex(1024);
        let (output, mut output_rx) = tokio::io::duplex(1024);
        let task =
            tokio::spawn(async move { carry(&route, "127.0.0.1", port, input, output).await });
        input_tx.write_all(b"ping\n").await.unwrap();
        input_tx.shutdown().await.unwrap();
        let mut seen = String::new();
        output_rx.read_to_string(&mut seen).await.unwrap();
        assert_eq!(seen, "hello\nping\n");
        task.await.unwrap().unwrap();
    }

    /// The reason the proxy gave is what the client that ran the tunnel shows.
    #[tokio::test]
    async fn a_refusal_is_returned_with_what_the_proxy_said() {
        let (_proxy, _lease, route) = route_for(false).await;
        let port = service().await;
        let (_input_tx, input) = tokio::io::duplex(64);
        let (output, _output_rx) = tokio::io::duplex(64);
        let said = carry(&route, "127.0.0.1", port, input, output)
            .await
            .unwrap_err();
        assert!(said.contains("lokode"), "{said}");
    }

    #[tokio::test]
    async fn a_proxy_that_cannot_be_reached_is_named() {
        let route = Route::parse("http://abc:x@127.0.0.1:1").unwrap();
        let (_input_tx, input) = tokio::io::duplex(64);
        let (output, _output_rx) = tokio::io::duplex(64);
        let said = carry(&route, "example.com", 22, input, output)
            .await
            .unwrap_err();
        assert!(said.contains("127.0.0.1:1"), "{said}");
    }
}
