//! The encrypted channel between a dashboard and a lokode that shares its sessions.
//!
//! Both ends hold the secret of the group.
//! The handshake is Noise `NNpsk0`: a peer without the secret cannot finish it, and the keys of
//! a connection are new each time.
//! What follows is JSON messages, each cut into frames of at most one Noise message.
//!
//! | Bytes | Meaning |
//! |---|---|
//! | 2, big endian | the length of the frame that follows |
//! | 1 | 1 on the last frame of a message, 0 on the others |
//! | the rest | a piece of the message |
//!
//! The 3 bytes ahead of the piece are encrypted with it.

use std::io;
use std::sync::Arc;

use super::noise::{Handshake, Transport};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;

/// The most a Noise message holds, tag included.
const FRAME: usize = 65535;

/// The tag Noise adds to every message.
const TAG: usize = super::noise::TAG;

/// The most one JSON message may take.
const MAX_MESSAGE: usize = 64 * 1024 * 1024;

/// Which machines belong together, and what proves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub name: String,
    pub secret: String,
}

impl Group {
    /// The 32 bytes that stand for the secret in the handshake.
    fn psk(&self) -> [u8; 32] {
        self.derive(b"lokode dashboard channel")
    }

    /// The key that signs what the group announces.
    pub fn announce_key(&self) -> [u8; 32] {
        self.derive(b"lokode dashboard announce")
    }

    fn derive(&self, purpose: &[u8]) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(purpose);
        for part in [self.name.as_bytes(), self.secret.as_bytes()] {
            hash.update((part.len() as u64).to_be_bytes());
            hash.update(part);
        }
        hash.finalize().into()
    }

    /// A handshake with a fresh ephemeral key, as one end or the other.
    fn handshake(&self, initiator: bool) -> io::Result<Handshake> {
        let mut ephemeral = [0u8; 32];
        getrandom::fill(&mut ephemeral).map_err(|e| io::Error::other(e.to_string()))?;
        Ok(Handshake::new(initiator, self.psk(), ephemeral))
    }
}

async fn write_frame(stream: &mut OwnedWriteHalf, frame: &[u8]) -> io::Result<()> {
    stream
        .write_all(&(frame.len() as u16).to_be_bytes())
        .await?;
    stream.write_all(frame).await
}

async fn read_frame(stream: &mut OwnedReadHalf) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 2];
    stream.read_exact(&mut len).await?;
    let mut frame = vec![0u8; u16::from_be_bytes(len) as usize];
    stream.read_exact(&mut frame).await?;
    Ok(frame)
}

/// Make a channel over `stream`, as the end that connected.
pub async fn connect(stream: TcpStream, group: &Group) -> io::Result<(Sender, Receiver)> {
    let mut handshake = group.handshake(true)?;
    let (mut read, mut write) = stream.into_split();
    let first = handshake
        .write_message(&[])
        .map_err(|e| io::Error::other(format!("{e:?}")))?;
    write_frame(&mut write, &first).await?;
    let answer = read_frame(&mut read).await?;
    handshake
        .read_message(&answer)
        .map_err(|_| io::Error::other("the other end does not hold the same secret"))?;
    finish(handshake, read, write)
}

/// Make a channel over `stream`, as the end that was connected to.
pub async fn accept(stream: TcpStream, group: &Group) -> io::Result<(Sender, Receiver)> {
    let mut handshake = group.handshake(false)?;
    let (mut read, mut write) = stream.into_split();
    let first = read_frame(&mut read).await?;
    handshake
        .read_message(&first)
        .map_err(|_| io::Error::other("the other end does not hold the same secret"))?;
    let answer = handshake
        .write_message(&[])
        .map_err(|e| io::Error::other(format!("{e:?}")))?;
    write_frame(&mut write, &answer).await?;
    finish(handshake, read, write)
}

fn finish(
    handshake: Handshake,
    read: OwnedReadHalf,
    write: OwnedWriteHalf,
) -> io::Result<(Sender, Receiver)> {
    let state = Arc::new(
        handshake
            .into_transport()
            .map_err(|e| io::Error::other(format!("{e:?}")))?,
    );
    Ok((
        Sender {
            write,
            state: state.clone(),
            nonce: 0,
        },
        Receiver {
            read,
            state,
            nonce: 0,
        },
    ))
}

/// The sending half of a channel.
pub struct Sender {
    write: OwnedWriteHalf,
    state: Arc<Transport>,
    nonce: u64,
}

impl Sender {
    /// Send one JSON message.
    pub async fn send(&mut self, message: &Value) -> io::Result<()> {
        let bytes = serde_json::to_vec(message)?;
        if bytes.len() > MAX_MESSAGE {
            return Err(io::Error::other("the message is too large"));
        }
        let piece = FRAME - TAG - 1;
        let mut pieces = bytes.chunks(piece).peekable();
        // An empty message is one empty piece.
        let empty: &[u8] = &[];
        let mut plain = Vec::with_capacity(FRAME);
        loop {
            let (chunk, last) = match pieces.next() {
                Some(chunk) => (chunk, pieces.peek().is_none()),
                None => (empty, true),
            };
            plain.clear();
            plain.push(last as u8);
            plain.extend_from_slice(chunk);
            let sealed = self.state.seal(self.nonce, &plain);
            self.nonce += 1;
            write_frame(&mut self.write, &sealed).await?;
            if last {
                return Ok(());
            }
        }
    }
}

/// The receiving half of a channel.
pub struct Receiver {
    read: OwnedReadHalf,
    state: Arc<Transport>,
    nonce: u64,
}

impl Receiver {
    /// The next JSON message, or `None` when the other end has closed.
    pub async fn recv(&mut self) -> io::Result<Option<Value>> {
        let mut message = Vec::new();
        loop {
            let frame = match read_frame(&mut self.read).await {
                Ok(frame) => frame,
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof && message.is_empty() => {
                    return Ok(None)
                }
                Err(e) => return Err(e),
            };
            let plain = self
                .state
                .open(self.nonce, &frame)
                .map_err(|_| io::Error::other("a frame could not be decrypted"))?;
            self.nonce += 1;
            let Some((&last, piece)) = plain.split_first() else {
                return Err(io::Error::other("a frame is empty"));
            };
            message.extend_from_slice(piece);
            if message.len() > MAX_MESSAGE {
                return Err(io::Error::other("the message is too large"));
            }
            if last == 1 {
                return Ok(Some(serde_json::from_slice(&message)?));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::net::TcpListener;

    fn group(secret: &str) -> Group {
        Group {
            name: "home".into(),
            secret: secret.into(),
        }
    }

    /// 2 ends of one connection, each holding the secret it is given.
    async fn pair(
        a: &Group,
        b: &Group,
    ) -> (
        io::Result<(Sender, Receiver)>,
        io::Result<(Sender, Receiver)>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = {
            let b = b.clone();
            tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                accept(stream, &b).await
            })
        };
        let client = connect(TcpStream::connect(addr).await.unwrap(), a).await;
        (client, server.await.unwrap())
    }

    #[tokio::test]
    async fn a_message_goes_both_ways_encrypted_with_the_same_secret() {
        let g = group("s3cret");
        let (client, server) = pair(&g, &g).await;
        let ((mut cs, mut cr), (mut ss, mut sr)) = (client.unwrap(), server.unwrap());
        cs.send(&json!({"hello": "server"})).await.unwrap();
        assert_eq!(
            sr.recv().await.unwrap().unwrap(),
            json!({"hello": "server"})
        );
        ss.send(&json!({"hello": "client"})).await.unwrap();
        assert_eq!(
            cr.recv().await.unwrap().unwrap(),
            json!({"hello": "client"})
        );
    }

    #[tokio::test]
    async fn a_message_larger_than_a_frame_arrives_whole() {
        let g = group("s3cret");
        let (client, server) = pair(&g, &g).await;
        let ((mut cs, _cr), (_ss, mut sr)) = (client.unwrap(), server.unwrap());
        let big = json!({"text": "x".repeat(300_000)});
        cs.send(&big).await.unwrap();
        assert_eq!(sr.recv().await.unwrap().unwrap(), big);
    }

    #[tokio::test]
    async fn another_secret_or_group_does_not_get_in() {
        let (client, server) = pair(&group("one"), &group("two")).await;
        assert!(server.is_err());
        // The end that connected learns nothing: its first read finds the other end closed.
        if let Ok((_, mut receiver)) = client {
            assert!(receiver.recv().await.is_err() || receiver.recv().await.unwrap().is_none());
        }
        let other = Group {
            name: "work".into(),
            secret: "one".into(),
        };
        let (_, server) = pair(&group("one"), &other).await;
        assert!(server.is_err());
    }

    #[tokio::test]
    async fn a_closed_channel_ends_the_receiver() {
        let g = group("s3cret");
        let (client, server) = pair(&g, &g).await;
        let (cs, _cr) = client.unwrap();
        let (_ss, mut sr) = server.unwrap();
        drop(cs);
        assert!(sr.recv().await.unwrap().is_none());
    }

    #[test]
    fn the_keys_for_the_channel_and_the_announcements_differ() {
        let g = group("s3cret");
        assert_ne!(g.psk(), g.announce_key());
        assert_ne!(g.psk(), group("other").psk());
    }
}
