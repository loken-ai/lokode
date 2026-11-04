//! Finding the lokode servers of a group without being told where they are.
//!
//! A server that shares its sessions announces itself on a multicast group, and a dashboard
//! listens.
//! An announcement carries the name of the group, the id of the server and its port, signed with
//! a key only the group holds.
//! The address is the one the datagram came from, so an announcement cannot send a dashboard to
//! a third machine.
//!
//! Multicast does not cross routers.
//! The addresses in `seeds` cover a network that has none.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use sha2::Sha256;

use super::wire::Group;

/// The multicast group announcements go to, inside the organisation.
pub const DISCOVERY_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 42, 100);
pub const DISCOVERY_PORT: u16 = 41998;

/// How often a server says it is there.
pub const ANNOUNCE_EVERY: Duration = Duration::from_secs(5);

/// How long a server stays listed after its last announcement.
pub const FORGET_AFTER: Duration = Duration::from_secs(20);

/// The first field of every announcement.
const MAGIC: &str = "lokode1";

/// What a server says about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Announcement {
    pub group: String,
    pub node: String,
    pub port: u16,
}

impl Announcement {
    /// The datagram: the fields, then their signature in hexadecimal.
    pub fn encode(&self, group: &Group) -> Vec<u8> {
        let body = self.body();
        format!("{body}\t{}", hex(&sign(group, &body))).into_bytes()
    }

    fn body(&self) -> String {
        format!("{MAGIC}\t{}\t{}\t{}", self.group, self.node, self.port)
    }

    /// The announcement in `bytes`, when it is of `group` and its signature holds.
    pub fn decode(bytes: &[u8], group: &Group) -> Option<Self> {
        let text = std::str::from_utf8(bytes).ok()?;
        let (body, signature) = text.rsplit_once('\t')?;
        let mut fields = body.split('\t');
        if fields.next()? != MAGIC {
            return None;
        }
        let (name, node, port) = (fields.next()?, fields.next()?, fields.next()?);
        if fields.next().is_some() || name != group.name || node.is_empty() {
            return None;
        }
        let mut mac = Hmac::<Sha256>::new_from_slice(&group.announce_key()).ok()?;
        mac.update(body.as_bytes());
        mac.verify_slice(&unhex(signature)?).ok()?;
        Some(Self {
            group: name.to_string(),
            node: node.to_string(),
            port: port.parse().ok()?,
        })
    }
}

fn sign(group: &Group, body: &str) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(&group.announce_key())
        .expect("a key of any length is accepted");
    mac.update(body.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

/// The servers heard of, by node id.
#[derive(Debug, Default)]
pub struct PeerBook {
    peers: HashMap<String, (SocketAddr, Instant)>,
}

impl PeerBook {
    /// Note an announcement that came from `from`, at `now`.
    ///
    /// Returns whether the server was not listed.
    pub fn learn(&mut self, a: &Announcement, from: IpAddr, now: Instant) -> bool {
        let addr = SocketAddr::new(from, a.port);
        self.peers.insert(a.node.clone(), (addr, now)).is_none()
    }

    /// The servers heard from recently, by node id.
    pub fn live(&mut self, now: Instant) -> Vec<(String, SocketAddr)> {
        self.peers
            .retain(|_, (_, seen)| now.duration_since(*seen) < FORGET_AFTER);
        let mut live: Vec<_> = self
            .peers
            .iter()
            .map(|(n, (a, _))| (n.clone(), *a))
            .collect();
        live.sort();
        live
    }
}

/// A socket on the discovery group, for announcing and for listening.
///
/// The address is shared, so a server and a dashboard on one machine hear each other.
pub fn socket() -> io::Result<std::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(all(unix, not(any(target_os = "solaris", target_os = "illumos"))))]
    socket.set_reuse_port(true)?;
    socket.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, DISCOVERY_PORT).into())?;
    let socket: std::net::UdpSocket = socket.into();
    socket.join_multicast_v4(&DISCOVERY_GROUP, &Ipv4Addr::UNSPECIFIED)?;
    socket.set_multicast_loop_v4(true)?;
    Ok(socket)
}

/// Say `a` to the group, once.
pub fn announce(socket: &std::net::UdpSocket, a: &Announcement, group: &Group) -> io::Result<()> {
    socket.send_to(
        &a.encode(group),
        SocketAddrV4::new(DISCOVERY_GROUP, DISCOVERY_PORT),
    )?;
    Ok(())
}

/// Announce `a` every few seconds, until the task is dropped.
pub fn announcing(a: Announcement, group: Group) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let socket = match socket() {
            Ok(s) => s,
            Err(e) => {
                tracing::info!("no multicast announcements ({e}); the seeds of the group still reach this server");
                return;
            }
        };
        loop {
            let _ = announce(&socket, &a, &group);
            tokio::time::sleep(ANNOUNCE_EVERY).await;
        }
    })
}

/// Listen for announcements of `group`, and keep the book up to date.
pub fn listening(
    group: Group,
) -> (
    std::sync::Arc<std::sync::Mutex<PeerBook>>,
    tokio::task::JoinHandle<()>,
) {
    let book = std::sync::Arc::new(std::sync::Mutex::new(PeerBook::default()));
    let kept = book.clone();
    let task = tokio::task::spawn_blocking(move || {
        let socket = match socket() {
            Ok(s) => s,
            Err(e) => {
                tracing::info!("no multicast discovery ({e}); seeds only");
                return;
            }
        };
        // A read that times out lets the task notice that the dashboard has gone.
        let _ = socket.set_read_timeout(Some(Duration::from_secs(1)));
        let mut buf = [0u8; 512];
        loop {
            match socket.recv_from(&mut buf) {
                Ok((n, from)) => {
                    if let Some(a) = Announcement::decode(&buf[..n], &group) {
                        if let Ok(mut book) = kept.lock() {
                            book.learn(&a, from.ip(), Instant::now());
                        }
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    if std::sync::Arc::strong_count(&kept) == 1 {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    });
    (book, task)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(secret: &str) -> Group {
        Group {
            name: "home".into(),
            secret: secret.into(),
        }
    }

    fn announcement() -> Announcement {
        Announcement {
            group: "home".into(),
            node: "n1".into(),
            port: 4000,
        }
    }

    #[test]
    fn an_announcement_of_the_group_is_read_back() {
        let g = group("s3cret");
        let bytes = announcement().encode(&g);
        assert_eq!(Announcement::decode(&bytes, &g), Some(announcement()));
    }

    #[test]
    fn another_secret_or_a_changed_field_is_ignored() {
        let bytes = announcement().encode(&group("s3cret"));
        assert_eq!(Announcement::decode(&bytes, &group("other")), None);
        let moved = String::from_utf8(bytes)
            .unwrap()
            .replace("4000", "4001")
            .into_bytes();
        assert_eq!(Announcement::decode(&moved, &group("s3cret")), None);
    }

    #[test]
    fn another_group_or_garbage_is_ignored() {
        let g = group("s3cret");
        let other = Group {
            name: "work".into(),
            ..g.clone()
        };
        let bytes = Announcement {
            group: "work".into(),
            ..announcement()
        }
        .encode(&other);
        assert_eq!(Announcement::decode(&bytes, &g), None);
        for junk in [
            &b""[..],
            b"loken1\tx",
            b"lokode1\thome\tn\t1\tzz",
            &[0xff, 0xfe],
        ] {
            assert_eq!(Announcement::decode(junk, &g), None);
        }
    }

    #[test]
    fn a_server_is_listed_for_a_while_and_then_forgotten() {
        let mut book = PeerBook::default();
        let now = Instant::now();
        let from: IpAddr = "192.0.2.7".parse().unwrap();
        assert!(book.learn(&announcement(), from, now));
        assert!(!book.learn(&announcement(), from, now));
        let live = book.live(now + Duration::from_secs(5));
        assert_eq!(
            live,
            [("n1".to_string(), "192.0.2.7:4000".parse().unwrap())]
        );
        assert!(book
            .live(now + FORGET_AFTER + Duration::from_secs(1))
            .is_empty());
    }
}
