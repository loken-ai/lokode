//! The proxy that every confined command reaches the network through.
//!
//! A confined command has no route of its own.
//! It is given the address of this proxy, and the proxy decides each connection.
//!
//! ```text
//! command (HTTP_PROXY, HTTPS_PROXY, ALL_PROXY)
//!    -> proxy: one decision per host, through the same gate as every other request
//!    -> the address the proxy resolved and checked itself
//! ```
//!
//! The decision is the approval the tools already use.
//! The configuration's `egress` rules, the agreements given with `Always` and the question to
//! the person all apply, in the order they apply to `web_fetch`.
//!
//! Each command holds a [`Lease`].
//! The lease names the command in the question, and a host is asked about once for as long as
//! the lease lives, however many connections the command opens to it.
//!
//! The proxy counts the bytes of every connection.
//! A connection that sends several times more than any earlier one to its host is paused and put
//! to the person.
//! Once it has received as much as the largest earlier upload, it must also send more for each
//! byte received than any earlier one did.
//! Before that the answer has not come, and the size alone decides.
//! What is usual comes from the connections already seen, kept for the project.
//!
//! The proxy sees the host and the port of a tunnel, and nothing of what travels in it.
//! A name is not resolved before the host has been agreed to, because a lookup is itself a
//! message that leaves the machine.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::Engine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;

use crate::sandbox::approval::{denial, Approval, ApprovalRequest, ApprovalScope};

/// The names a command reaches directly instead of through the proxy.
const LOCAL: &str = "localhost,127.0.0.1,::1";

/// How long a connection may take to send its request head or its SOCKS greeting.
#[cfg(not(test))]
const HEAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Short enough for a test to wait for.
#[cfg(test)]
const HEAD_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(200);

/// The bytes a request head may take before the connection is refused.
const MAX_HEAD_BYTES: usize = 64 * 1024;

/// What settles whether a command may reach a host.
#[async_trait]
pub trait Gate: Send + Sync {
    async fn approve(&self, req: ApprovalRequest) -> Approval;

    /// Settle a host without asking anyone, and remember it as agreed to.
    ///
    /// A rule that refuses the host still does.
    /// A gate that cannot remember asks as usual.
    async fn record(&self, req: ApprovalRequest) -> Approval {
        self.approve(req).await
    }
}

/// A gate that records the hosts instead of asking about them.
///
/// Every other question, such as an upload larger than usual, goes through as it was.
pub struct Recording(pub Arc<dyn Gate>);

#[async_trait]
impl Gate for Recording {
    async fn approve(&self, req: ApprovalRequest) -> Approval {
        match req.scope {
            ApprovalScope::Host(_) => self.0.record(req).await,
            _ => self.0.approve(req).await,
        }
    }
}

#[async_trait]
impl Gate for crate::tools::registry::ToolCtx {
    async fn approve(&self, req: ApprovalRequest) -> Approval {
        crate::tools::registry::ToolCtx::approve(self, req).await
    }

    async fn record(&self, req: ApprovalRequest) -> Approval {
        crate::tools::registry::ToolCtx::record(self, req).await
    }
}

/// What a session has to filter the network of its commands: the proxy, and where it lives.
///
/// On Linux the proxy listens inside a network namespace that has a loopback and no other route.
/// A command that joins the namespaces reaches the proxy and nothing else.
/// Elsewhere the proxy listens on this machine's loopback, and the sandbox profile lets a
/// command reach that address alone.
pub struct Egress {
    proxy: Proxy,
    #[cfg(target_os = "linux")]
    user: std::os::fd::OwnedFd,
    #[cfg(target_os = "linux")]
    net: std::os::fd::OwnedFd,
    #[cfg(target_os = "linux")]
    mount: std::os::fd::OwnedFd,
}

impl Egress {
    /// Start the proxy of a session.
    ///
    /// On Linux this fails where unprivileged user namespaces are not available.
    pub async fn start(
        allow_hosts: Arc<HashSet<String>>,
        journal: Arc<Journal>,
    ) -> std::io::Result<Self> {
        #[cfg(target_os = "linux")]
        let made = netns::create(firewall().as_deref())?;
        #[cfg(target_os = "linux")]
        let (listener, dns) = (made.listener, made.dns);
        #[cfg(not(target_os = "linux"))]
        let (listener, dns) = (std::net::TcpListener::bind("127.0.0.1:0")?, None);
        listener.set_nonblocking(true)?;
        let dns = match dns {
            Some(socket) => {
                socket.set_nonblocking(true)?;
                Some(tokio::net::UdpSocket::from_std(socket)?)
            }
            None => None,
        };
        let proxy = Proxy::serve_with(TcpListener::from_std(listener)?, dns, allow_hosts, journal)?;
        Ok(Self {
            proxy,
            #[cfg(target_os = "linux")]
            user: made.user,
            #[cfg(target_os = "linux")]
            net: made.net,
            #[cfg(target_os = "linux")]
            mount: made.mount,
        })
    }

    /// A lease for one command, decided by `gate` and named `origin` in the question.
    pub fn lease(&self, gate: Arc<dyn Gate>, origin: impl Into<String>) -> Lease {
        self.proxy.lease(gate, origin)
    }

    /// Whether a connection that ignores the proxy variables is carried too.
    ///
    /// Then the namespace redirects every connection to the proxy, and answers the name lookups
    /// itself.
    pub fn redirects(&self) -> bool {
        self.proxy.inner.redirect
    }

    /// The port the proxy listens on.
    pub fn port(&self) -> u16 {
        self.proxy.local_addr().port()
    }

    /// The user, network and mount namespaces a command joins, in that order, when this system
    /// has them.
    pub fn namespaces(&self) -> Option<[std::os::fd::RawFd; 3]> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            Some([
                self.user.as_raw_fd(),
                self.net.as_raw_fd(),
                self.mount.as_raw_fd(),
            ])
        }
        #[cfg(not(target_os = "linux"))]
        None
    }
}

impl std::fmt::Debug for Egress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Egress")
            .field("addr", &self.proxy.local_addr())
            .finish()
    }
}

/// Two sessions never share a proxy, so identity is equality.
impl PartialEq for Egress {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}

impl Eq for Egress {}

/// The program that sets the firewall rules of a namespace, when this machine has one.
///
/// Only a program of the system is taken: it belongs to root and nobody else may write it.
/// The search path is not read, since a command may have planted a program on it.
#[cfg(target_os = "linux")]
fn firewall() -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    ["/usr/sbin/nft", "/sbin/nft", "/usr/bin/nft"]
        .into_iter()
        .map(PathBuf::from)
        .find(|candidate| {
            std::fs::metadata(candidate)
                .is_ok_and(|m| m.is_file() && m.uid() == 0 && m.mode() & 0o022 == 0)
        })
}

/// The namespaces of a session.
#[cfg(target_os = "linux")]
mod netns {
    use std::ffi::CString;
    use std::fs::File;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::process::CommandExt;
    use std::path::Path;
    use std::process::{Command, Stdio};

    /// The user namespace and the network namespace a command joins, and the proxy's sockets.
    pub struct Made {
        pub user: OwnedFd,
        pub net: OwnedFd,
        /// The mount namespace, in which the sockets of the machine's services are out of reach.
        pub mount: OwnedFd,
        pub listener: std::net::TcpListener,
        /// The socket that answers name lookups, when the namespace redirects the connections.
        pub dns: Option<std::net::UdpSocket>,
    }

    /// What the helper runs once it is in its namespace.
    ///
    /// With a firewall program given, every TCP connection that does not stay on the loopback
    /// is redirected to the proxy, and every name lookup to the resolver.
    /// It says `redirected` when the rules are in place and `ready` when it was given no program.
    const SETUP: &str = r#"
if [ -n "$1" ]; then
  if "$1" -f - <<RULES
table ip lokode {
  chain out {
    type nat hook output priority -100;
    udp dport 53 redirect to :$3
    ip daddr 127.0.0.0/8 return
    ip protocol tcp redirect to :$2
  }
}
RULES
  then echo redirected; else echo failed; fi
else
  echo ready
fi
read line
"#;

    /// The services of the machine that a command could ask to run something outside the
    /// sandbox, reached through a socket in the file system.
    ///
    /// The network namespace cuts the network and leaves these alone, and a program that reaches
    /// one of them is started outside every limit the sandbox sets.
    /// Inside the mount namespace each is covered by an empty directory, or by an empty file.
    /// The runtime directory of the machine holds the session bus, the user's service manager,
    /// the container engine and the key agents.
    /// The temporary directory holds the display, the terminal multiplexer and the agents of a
    /// session, and any other socket that is there when the session starts.
    struct Hidden {
        /// Directories that are covered, and that must be.
        required: Vec<CString>,
        /// Directories that are covered when they can be.
        directories: Vec<CString>,
        /// Sockets that are covered by an empty file when they can be.
        sockets: Vec<CString>,
    }

    impl Hidden {
        fn find() -> Self {
            use std::os::unix::fs::FileTypeExt;
            let path = |p: &Path| CString::new(p.as_os_str().as_bytes()).ok();
            let mut required: Vec<CString> = Vec::new();
            for dir in ["/run", "/var/run"] {
                let dir = Path::new(dir);
                // A link to the other one is covered through it.
                let real = std::fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir());
                if let (true, Some(c)) = (real, path(dir)) {
                    required.push(c);
                }
            }
            let mut directories = Vec::new();
            let mut sockets = Vec::new();
            if let Ok(entries) = std::fs::read_dir("/tmp") {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    let Ok(kind) = entry.file_type() else {
                        continue;
                    };
                    let known = [
                        ".X11-unix",
                        ".ICE-unix",
                        "ssh-",
                        "tmux-",
                        "keyring-",
                        "gpg-",
                    ]
                    .iter()
                    .any(|prefix| name.starts_with(prefix));
                    match (kind.is_dir(), kind.is_socket()) {
                        (true, _) if known => directories.extend(path(&entry.path())),
                        (_, true) => sockets.extend(path(&entry.path())),
                        _ => {}
                    }
                }
            }
            Self {
                required,
                directories,
                sockets,
            }
        }

        /// Cover them, in the mount namespace of the calling process.
        ///
        /// Runs in the child, between `fork` and `exec`: syscalls on buffers made before the fork.
        fn apply(&self) -> io::Result<()> {
            // SAFETY: plain mount calls whose strings end in a nul and outlive each call.
            unsafe {
                // What is mounted here does not reach the machine's own mount namespace.
                if libc::mount(
                    std::ptr::null(),
                    c"/".as_ptr(),
                    std::ptr::null(),
                    libc::MS_REC | libc::MS_PRIVATE,
                    std::ptr::null(),
                ) != 0
                {
                    return Err(io::Error::last_os_error());
                }
                let cover = |dir: &CString| {
                    libc::mount(
                        c"tmpfs".as_ptr(),
                        dir.as_ptr(),
                        c"tmpfs".as_ptr(),
                        libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
                        c"mode=0755".as_ptr().cast(),
                    )
                };
                for dir in &self.required {
                    if cover(dir) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                for dir in &self.directories {
                    cover(dir);
                }
                for file in &self.sockets {
                    libc::mount(
                        c"/dev/null".as_ptr(),
                        file.as_ptr(),
                        std::ptr::null(),
                        libc::MS_BIND,
                        std::ptr::null(),
                    );
                }
            }
            Ok(())
        }
    }

    /// Make the namespaces, with the redirection of the connections when `firewall` can set it
    /// up and without it when not.
    pub fn create(firewall: Option<&Path>) -> io::Result<Made> {
        if let Some(program) = firewall {
            if let Ok(made) = make(Some(program)) {
                return Ok(made);
            }
        }
        make(None)
    }

    /// A port that is free on this machine's loopback.
    ///
    /// The namespace is new, so the same number is free in it.
    fn free_ports() -> io::Result<(u16, u16)> {
        let tcp = std::net::TcpListener::bind("127.0.0.1:0")?;
        let udp = std::net::UdpSocket::bind("127.0.0.1:0")?;
        Ok((tcp.local_addr()?.port(), udp.local_addr()?.port()))
    }

    /// Make a user namespace that maps the caller's own identity, and a network namespace in it
    /// that has a loopback and nothing else, with a listener bound on that loopback.
    ///
    /// A process cannot enter a user namespace once it has threads, so a child makes them.
    /// The child is a shell that waits, and the session keeps descriptors on its namespaces and
    /// on the sockets it opened.
    fn make(firewall: Option<&Path>) -> io::Result<Made> {
        // SAFETY: plain calls that return an identity.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getegid()) };
        let uid_map = format!("{uid} {uid} 1\n");
        let gid_map = format!("{gid} {gid} 1\n");
        let (tcp_port, udp_port) = free_ports()?;
        let redirect = firewall.is_some();
        let hidden = Hidden::find();

        let mut child = Command::new("/bin/sh");
        child
            .args(["-c", SETUP, "sh"])
            .arg(firewall.map(Path::as_os_str).unwrap_or_default())
            .arg(tcp_port.to_string())
            .arg(udp_port.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // SAFETY: the closure makes syscalls on buffers made before the fork and nothing else.
        // It allocates nothing and takes no lock, which is what a forked child may do.
        unsafe {
            child.pre_exec(move || {
                let spaces = libc::CLONE_NEWUSER | libc::CLONE_NEWNET | libc::CLONE_NEWNS;
                if libc::unshare(spaces) != 0 {
                    return Err(io::Error::last_os_error());
                }
                write_file(c"/proc/self/setgroups", b"deny")?;
                write_file(c"/proc/self/uid_map", uid_map.as_bytes())?;
                write_file(c"/proc/self/gid_map", gid_map.as_bytes())?;
                hidden.apply()?;
                loopback_up()?;
                if redirect {
                    keep_net_admin()?;
                    route_everything_to_loopback()?;
                    open_loopback(libc::SOCK_DGRAM, udp_port)?;
                }
                open_loopback(libc::SOCK_STREAM, tcp_port)
            });
        }
        let mut child = child
            .spawn()
            .map_err(|e| io::Error::new(e.kind(), format!("spawn: {e}")))?;
        // The child says so once it has run, which is after `exec` has closed what it inherited.
        let mut said = String::new();
        if let Some(out) = child.stdout.take() {
            let _ = std::io::BufRead::read_line(&mut std::io::BufReader::new(out), &mut said);
        }
        let expected = if redirect { "redirected" } else { "ready" };
        let made = match said.trim() {
            s if s == expected => take(&child, redirect),
            _ => Err(io::Error::other("the namespace helper did not start")),
        };
        drop(child.stdin.take());
        let _ = child.wait();
        made
    }

    /// What the waiting child holds: its namespaces and its sockets.
    fn take(child: &std::process::Child, redirect: bool) -> io::Result<Made> {
        let pid = child.id();
        let at = |step: &'static str| {
            move |e: io::Error| io::Error::new(e.kind(), format!("{step}: {e}"))
        };
        let user: OwnedFd = File::open(format!("/proc/{pid}/ns/user"))
            .map_err(at("user ns"))?
            .into();
        let net: OwnedFd = File::open(format!("/proc/{pid}/ns/net"))
            .map_err(at("net ns"))?
            .into();
        let mount: OwnedFd = File::open(format!("/proc/{pid}/ns/mnt"))
            .map_err(at("mount ns"))?
            .into();
        // SAFETY: the call only reads its arguments, and a negative result is an error.
        let pidfd = unsafe {
            let pidfd = libc::syscall(libc::SYS_pidfd_open, pid as libc::c_int, 0);
            if pidfd < 0 {
                return Err(io::Error::last_os_error());
            }
            OwnedFd::from_raw_fd(pidfd as i32)
        };
        let mut listener = None;
        let mut dns = None;
        for entry in std::fs::read_dir(format!("/proc/{pid}/fd")).map_err(at("fd dir"))? {
            let entry = entry.map_err(at("fd entry"))?;
            let Ok(target) = std::fs::read_link(entry.path()) else {
                continue;
            };
            if !target.to_string_lossy().starts_with("socket:") {
                continue;
            }
            let Some(number) = entry.file_name().to_string_lossy().parse::<i32>().ok() else {
                continue;
            };
            let own = steal(&pidfd, number)?;
            match socket_type(&own)? {
                libc::SOCK_STREAM => listener = Some(std::net::TcpListener::from(own)),
                libc::SOCK_DGRAM => dns = Some(std::net::UdpSocket::from(own)),
                _ => {}
            }
        }
        let listener = listener.ok_or_else(|| io::Error::other("the namespace has no listener"))?;
        if redirect && dns.is_none() {
            return Err(io::Error::other("the namespace has no resolver socket"));
        }
        Ok(Made {
            user,
            net,
            mount,
            listener,
            dns: dns.filter(|_| redirect),
        })
    }

    /// A copy of a descriptor of the process `pidfd` names.
    fn steal(pidfd: &OwnedFd, number: i32) -> io::Result<OwnedFd> {
        // SAFETY: the call only reads its arguments, and a negative result is an error.
        unsafe {
            let fd = libc::syscall(libc::SYS_pidfd_getfd, pidfd.as_raw_fd(), number, 0);
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(OwnedFd::from_raw_fd(fd as i32))
        }
    }

    /// The type of a socket: stream or datagram.
    fn socket_type(fd: &OwnedFd) -> io::Result<libc::c_int> {
        let mut kind: libc::c_int = 0;
        let mut size = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: the pointers name a live integer and its size.
        let rc = unsafe {
            libc::getsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_TYPE,
                (&mut kind as *mut libc::c_int).cast(),
                &mut size,
            )
        };
        match rc {
            0 => Ok(kind),
            _ => Err(io::Error::last_os_error()),
        }
    }

    /// Write `bytes` to a file of `/proc`.
    fn write_file(path: &std::ffi::CStr, bytes: &[u8]) -> io::Result<()> {
        // SAFETY: `path` ends in a nul and `bytes` is live for the call.
        unsafe {
            let fd = libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let wrote = libc::write(fd, bytes.as_ptr().cast(), bytes.len());
            let err = io::Error::last_os_error();
            libc::close(fd);
            match wrote as usize == bytes.len() {
                true => Ok(()),
                false => Err(err),
            }
        }
    }

    /// Bring `lo` up in the namespace of the calling process.
    fn loopback_up() -> io::Result<()> {
        // SAFETY: plain syscalls on a socket opened here and a zeroed request.
        unsafe {
            let sock = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
            if sock < 0 {
                return Err(io::Error::last_os_error());
            }
            let mut req: libc::ifreq = std::mem::zeroed();
            for (slot, byte) in req.ifr_name.iter_mut().zip(b"lo\0") {
                *slot = *byte as libc::c_char;
            }
            req.ifr_ifru.ifru_flags = (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
            let rc = libc::ioctl(sock, libc::SIOCSIFFLAGS, &req);
            let err = io::Error::last_os_error();
            libc::close(sock);
            match rc {
                0 => Ok(()),
                _ => Err(err),
            }
        }
    }

    /// Give a program the helper starts the right to change the firewall of its namespace.
    ///
    /// A program run by a user that is not root loses its capabilities when it starts, unless
    /// they are ambient.
    fn keep_net_admin() -> io::Result<()> {
        #[repr(C)]
        struct Header {
            version: u32,
            pid: i32,
        }
        #[repr(C)]
        #[derive(Clone, Copy)]
        struct Data {
            effective: u32,
            permitted: u32,
            inheritable: u32,
        }
        // Capability set version 3, and the number of CAP_NET_ADMIN.
        const VERSION: u32 = 0x2008_0522;
        const NET_ADMIN: u32 = 12;
        let mut header = Header {
            version: VERSION,
            pid: 0,
        };
        let mut data = [Data {
            effective: 0,
            permitted: 0,
            inheritable: 0,
        }; 2];
        // SAFETY: the structures are live and laid out as the kernel reads them.
        unsafe {
            if libc::syscall(libc::SYS_capget, &mut header, data.as_mut_ptr()) != 0 {
                return Err(io::Error::last_os_error());
            }
            data[0].inheritable |= 1 << NET_ADMIN;
            if libc::syscall(libc::SYS_capset, &mut header, data.as_ptr()) != 0 {
                return Err(io::Error::last_os_error());
            }
            let rc = libc::prctl(
                libc::PR_CAP_AMBIENT,
                libc::PR_CAP_AMBIENT_RAISE as libc::c_ulong,
                NET_ADMIN as libc::c_ulong,
                0,
                0,
            );
            match rc {
                0 => Ok(()),
                _ => Err(io::Error::last_os_error()),
            }
        }
    }

    /// Add a route that makes every address local to the namespace.
    ///
    /// Without a route a connection to an outside address fails before the firewall sees it.
    /// A local route also gives the connection a source address that the proxy can answer.
    fn route_everything_to_loopback() -> io::Result<()> {
        const RTM_NEWROUTE: u16 = 24;
        const REQUEST_CREATE_EXCL_ACK: u16 = 0x1 | 0x400 | 0x200 | 0x4;
        const MAIN_TABLE: u8 = 254;
        const BOOT: u8 = 3;
        const HOST_SCOPE: u8 = 254;
        const LOCAL_ROUTE: u8 = 2;
        const OUTPUT_INTERFACE: u16 = 4;
        // The loopback is the first interface of a new namespace.
        const LOOPBACK_INDEX: u32 = 1;

        // A header, a route message of the main table and the interface to send through.
        let mut message = [0u8; 36];
        message[0..4].copy_from_slice(&36u32.to_ne_bytes());
        message[4..6].copy_from_slice(&RTM_NEWROUTE.to_ne_bytes());
        message[6..8].copy_from_slice(&REQUEST_CREATE_EXCL_ACK.to_ne_bytes());
        message[16] = libc::AF_INET as u8;
        message[20] = MAIN_TABLE;
        message[21] = BOOT;
        message[22] = HOST_SCOPE;
        message[23] = LOCAL_ROUTE;
        message[28..30].copy_from_slice(&8u16.to_ne_bytes());
        message[30..32].copy_from_slice(&OUTPUT_INTERFACE.to_ne_bytes());
        message[32..36].copy_from_slice(&LOOPBACK_INDEX.to_ne_bytes());

        // SAFETY: plain syscalls on a socket opened here and buffers that outlive each call.
        unsafe {
            let sock = libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                libc::NETLINK_ROUTE,
            );
            if sock < 0 {
                return Err(io::Error::last_os_error());
            }
            let sent = libc::send(sock, message.as_ptr().cast(), message.len(), 0);
            let mut reply = [0u8; 64];
            let got = match sent {
                n if n < 0 => -1,
                _ => libc::recv(sock, reply.as_mut_ptr().cast(), reply.len(), 0),
            };
            let err = io::Error::last_os_error();
            libc::close(sock);
            if got < 20 {
                return Err(err);
            }
            // The acknowledgement carries 0, or a negated error number, after the header.
            let code = i32::from_ne_bytes([reply[16], reply[17], reply[18], reply[19]]);
            match code {
                0 => Ok(()),
                code => Err(io::Error::from_raw_os_error(-code)),
            }
        }
    }

    /// Open a socket of `kind` on the loopback at `port`, which the process keeps across `exec`.
    ///
    /// A stream socket also listens.
    fn open_loopback(kind: libc::c_int, port: u16) -> io::Result<()> {
        // SAFETY: plain syscalls on a socket opened here and a zeroed address.
        unsafe {
            let sock = libc::socket(libc::AF_INET, kind, 0);
            if sock < 0 {
                return Err(io::Error::last_os_error());
            }
            let mut addr: libc::sockaddr_in = std::mem::zeroed();
            addr.sin_family = libc::AF_INET as libc::sa_family_t;
            addr.sin_port = port.to_be();
            addr.sin_addr.s_addr = u32::from_ne_bytes([127, 0, 0, 1]);
            let size = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
            let bound = libc::bind(sock, (&addr as *const libc::sockaddr_in).cast(), size);
            let listening = kind != libc::SOCK_STREAM || libc::listen(sock, 128) == 0;
            if bound != 0 || !listening {
                let err = io::Error::last_os_error();
                libc::close(sock);
                return Err(err);
            }
            Ok(())
        }
    }
}

/// The proxy of one session.
///
/// It stops listening when dropped.
pub struct Proxy {
    inner: Arc<Inner>,
}

struct Inner {
    addr: SocketAddr,
    journal: Arc<Journal>,
    leases: Mutex<HashMap<String, Arc<Session>>>,
    /// Hosts reachable although they resolve to a private address.
    allow_hosts: Arc<HashSet<String>>,
    /// Whether connections arrive that were redirected here and never asked for the proxy.
    redirect: bool,
    /// The names that were handed out as addresses.
    names: Mutex<Names>,
    stop: CancellationToken,
}

/// What the proxy has seen of one host.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Seen {
    pub connections: u64,
    pub refused: u64,
    pub sent: u64,
    pub received: u64,
    /// The ports connections were made to.
    pub ports: std::collections::BTreeSet<u16>,
    /// Why the last connection was refused, when one was.
    pub last_refusal: Option<String>,
}

/// What is usual for a host: the largest of what its connections did.
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Usual {
    /// The most bytes one connection sent.
    pub sent: u64,
    /// The highest bytes sent for each byte received.
    pub ratio: f64,
    /// The most bytes one session sent to the host in all.
    #[serde(default)]
    pub session: u64,
}

/// Bytes sent for each byte received, with a connection that received nothing counted as 1.
fn ratio(sent: u64, received: u64) -> f64 {
    sent as f64 / received.max(1) as f64
}

/// What the proxy has seen, for `/net` and for telling the usual from the unusual.
#[derive(Debug, Default)]
pub struct Journal {
    seen: Mutex<BTreeMap<String, Seen>>,
    usual: Mutex<BTreeMap<String, Usual>>,
    /// The project the usual is kept for between sessions, when it is.
    kept: Option<PathBuf>,
    /// The most bytes one earlier session sent to each host, as they were when this one began.
    earlier: BTreeMap<String, u64>,
    /// Counts the states of the usual, so that an older one never replaces a newer one on disk.
    generation: AtomicU64,
    /// The latest state written.
    written: Arc<Mutex<u64>>,
}

impl Journal {
    /// The journal of the project at `root`, with the usual it has learned in earlier sessions.
    pub fn kept_for(root: &Path) -> Self {
        let usual = read_usual_all().and_then(|mut all| all.remove(&root.display().to_string()));
        let usual = usual.unwrap_or_default();
        Self {
            earlier: usual.iter().map(|(h, u)| (h.clone(), u.session)).collect(),
            usual: Mutex::new(usual),
            kept: Some(root.to_path_buf()),
            ..Self::default()
        }
    }

    /// What each host has seen of this session, by name.
    pub fn seen(&self) -> Vec<(String, Seen)> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|(h, s)| (h.clone(), s.clone()))
            .collect()
    }

    /// What is usual, by host.
    pub fn usual(&self) -> Vec<(String, Usual)> {
        self.usual
            .lock()
            .unwrap()
            .iter()
            .map(|(h, u)| (h.clone(), *u))
            .collect()
    }

    /// What is usual for `host`.
    ///
    /// A host never seen is judged by the largest of every host seen.
    /// `None` before anything has been seen, which the proxy reads as an upload of nothing.
    pub fn usual_for(&self, host: &str) -> Option<Usual> {
        let all = self.usual.lock().unwrap();
        if let Some(u) = all.get(host) {
            return Some(*u);
        }
        all.values().copied().reduce(|a, b| Usual {
            sent: a.sent.max(b.sent),
            ratio: a.ratio.max(b.ratio),
            session: a.session.max(b.session),
        })
    }

    /// What this session has sent to `host` so far, and how much it may send before it is put
    /// to the person.
    ///
    /// The limit comes from what earlier sessions sent to the host, or to any host for one never
    /// seen.
    /// `None` before any earlier session, since nothing is unusual then.
    pub(crate) fn session_budget(&self, host: &str) -> Option<(u64, u64)> {
        let before = self
            .earlier
            .get(host)
            .or_else(|| self.earlier.values().max())
            .copied()
            .filter(|before| *before > 0)?;
        let already = self.seen.lock().unwrap().get(host).map_or(0, |s| s.sent);
        Some((already, before.saturating_mul(UNUSUAL_FACTOR)))
    }

    /// A connection asked for `port` of `host`.
    pub(crate) fn reached(&self, host: &str, port: u16) {
        let mut seen = self.seen.lock().unwrap();
        seen.entry(host.to_string()).or_default().ports.insert(port);
    }

    /// A connection to `host` that carried `sent` and `received` bytes.
    pub(crate) fn connected(&self, host: &str, sent: u64, received: u64) {
        let mut seen = self.seen.lock().unwrap();
        let entry = seen.entry(host.to_string()).or_default();
        entry.connections += 1;
        entry.sent += sent;
        entry.received += received;
        let session = entry.sent;
        drop(seen);
        let mut usual = self.usual.lock().unwrap();
        let entry = usual.entry(host.to_string()).or_default();
        let before = *entry;
        entry.sent = entry.sent.max(sent);
        entry.session = entry.session.max(session);
        entry.ratio = entry.ratio.max(ratio(sent, received));
        if *entry != before {
            self.keep(&usual);
        }
    }

    /// A connection to `host` that was turned away.
    fn refused(&self, host: &str, why: &str) {
        let mut seen = self.seen.lock().unwrap();
        let entry = seen.entry(host.to_string()).or_default();
        entry.refused += 1;
        entry.last_refusal = Some(why.to_string());
    }

    /// Write what is usual into the store of the project it is kept for.
    ///
    /// The file is written away from the connection that changed it.
    fn keep(&self, held: &BTreeMap<String, Usual>) {
        let Some(root) = self.kept.clone() else {
            return;
        };
        let snapshot = held.clone();
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let written = self.written.clone();
        let job = move || write_usual(&root, &snapshot, generation, &written);
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn_blocking(job);
            }
            Err(_) => job(),
        }
    }
}

/// Where what is usual is kept, by project.
fn usual_path() -> Option<PathBuf> {
    crate::paths::data_dir().map(|d| d.join("egress-history.json"))
}

/// What every project has learned, or None where the file exists and cannot be read.
///
/// A file that cannot be read is left alone, so that one bad write does not erase the history
/// of every project.
fn read_usual_all() -> Option<BTreeMap<String, BTreeMap<String, Usual>>> {
    let Some(path) = usual_path() else {
        return Some(BTreeMap::new());
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).ok(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(BTreeMap::new()),
        Err(_) => None,
    }
}

/// Replace the usual of the project at `root` in the shared file, unless a newer state was
/// written already.
///
/// What another session wrote for the same project is kept where it is larger.
/// The file is locked while it is read and replaced, so that 2 sessions never lose each other's
/// update.
/// The file is written aside and moved into place, so that a reader never sees half of it.
fn write_usual(root: &Path, held: &BTreeMap<String, Usual>, generation: u64, written: &Mutex<u64>) {
    let Some(path) = usual_path() else {
        return;
    };
    let mut latest = written.lock().unwrap();
    if generation <= *latest {
        return;
    }
    let _lock = match lock_usual(&path) {
        Ok(lock) => lock,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "network habits not kept");
            return;
        }
    };
    let Some(mut all) = read_usual_all() else {
        tracing::warn!(path = %path.display(), "network habits not kept: the file cannot be read");
        return;
    };
    let mut merged = held.clone();
    for (host, other) in all.get(&root.display().to_string()).into_iter().flatten() {
        let mine = merged.entry(host.clone()).or_insert(*other);
        mine.sent = mine.sent.max(other.sent);
        mine.ratio = mine.ratio.max(other.ratio);
        mine.session = mine.session.max(other.session);
    }
    all.insert(root.display().to_string(), merged);
    let aside = path.with_extension("json.new");
    let done = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|_| {
            std::fs::write(
                &aside,
                serde_json::to_string_pretty(&all).unwrap_or_default(),
            )
        })
        .and_then(|_| std::fs::rename(&aside, &path));
    match done {
        Ok(()) => *latest = generation,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "network habits not kept");
        }
    }
}

/// Hold the lock of the shared file until the returned file is dropped.
fn lock_usual(path: &Path) -> std::io::Result<std::fs::File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path.with_extension("json.lock"))?;
    lock.lock()?;
    Ok(lock)
}

/// Bytes as a person reads them.
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    match unit {
        0 => format!("{bytes} B"),
        _ => format!("{value:.1} {}", UNITS[unit]),
    }
}

/// The answer to a question about a host: agreed to, or refused for the reason given.
type Answer = Result<(), String>;

/// What one command is allowed to ask for.
struct Session {
    gate: Arc<dyn Gate>,
    /// Who connects, in the words of the question.
    origin: String,
    /// The answer for each host, settled once.
    hosts: Mutex<HashMap<String, Arc<OnceCell<Answer>>>>,
    /// What was turned away, in the words the command can be told.
    turned_away: Mutex<Vec<String>>,
    /// Cancelled when the lease ends, which ends the command's connections.
    done: CancellationToken,
}

impl Proxy {
    /// Serve connections accepted on `listener` until the proxy is dropped.
    pub fn serve(
        listener: TcpListener,
        allow_hosts: Arc<HashSet<String>>,
        journal: Arc<Journal>,
    ) -> std::io::Result<Self> {
        Self::serve_with(listener, None, allow_hosts, journal)
    }

    /// Serve as [`Proxy::serve`] does, and carry the connections redirected to `listener` when
    /// `dns` is given.
    ///
    /// `dns` answers the name lookups with addresses of its own, so that a redirected connection
    /// can be told by the name it was made for.
    pub fn serve_with(
        listener: TcpListener,
        dns: Option<tokio::net::UdpSocket>,
        allow_hosts: Arc<HashSet<String>>,
        journal: Arc<Journal>,
    ) -> std::io::Result<Self> {
        let inner = Arc::new(Inner {
            addr: listener.local_addr()?,
            journal,
            leases: Mutex::new(HashMap::new()),
            allow_hosts,
            redirect: dns.is_some(),
            names: Mutex::new(Names::default()),
            stop: CancellationToken::new(),
        });
        if let Some(socket) = dns {
            tokio::spawn(resolve(inner.clone(), socket));
        }
        let accepting = inner.clone();
        tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    biased;
                    _ = accepting.stop.cancelled() => return,
                    a = listener.accept() => a,
                };
                match accepted {
                    Ok((client, _)) => {
                        let inner = accepting.clone();
                        // A connection redirected here still carries the address it was made for.
                        // One made to the proxy itself carries the proxy's own address.
                        let made_for = original_destination(&client)
                            .filter(|to| inner.redirect && *to != inner.addr);
                        match made_for {
                            Some(to) => tokio::spawn(redirected(inner, client, to)),
                            None => tokio::spawn(connection(inner, client)),
                        };
                    }
                    Err(e) => tracing::warn!(error = %e, "proxy accept failed"),
                }
            }
        });
        Ok(Self { inner })
    }

    /// Where the proxy listens.
    pub fn local_addr(&self) -> SocketAddr {
        self.inner.addr
    }

    /// A lease for one command, decided by `gate` and named `origin` in the question.
    pub fn lease(&self, gate: Arc<dyn Gate>, origin: impl Into<String>) -> Lease {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).expect("the system gives no random bytes");
        let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let session = Arc::new(Session {
            gate,
            origin: origin.into(),
            hosts: Mutex::new(HashMap::new()),
            turned_away: Mutex::new(Vec::new()),
            done: CancellationToken::new(),
        });
        self.inner
            .leases
            .lock()
            .unwrap()
            .insert(token.clone(), session.clone());
        Lease {
            token,
            inner: self.inner.clone(),
            session,
        }
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.inner.stop.cancel();
    }
}

/// The right of one command to go through the proxy, for as long as it is held.
pub struct Lease {
    token: String,
    inner: Arc<Inner>,
    session: Arc<Session>,
}

impl Lease {
    /// The connections of this command that were turned away, and why.
    pub fn turned_away(&self) -> Vec<String> {
        self.session.turned_away.lock().unwrap().clone()
    }

    /// The environment that has a command use the proxy.
    ///
    /// The token travels as the user name of the proxy address.
    /// HTTP clients get an HTTP proxy, and `ALL_PROXY` names SOCKS5.
    /// An inherited `NO_PROXY` is replaced by this machine's own names.
    /// What a command reaches on its loopback never leaves the sandbox.
    pub fn env(&self) -> Vec<(String, String)> {
        let url = format!("http://{}:x@{}", self.token, self.inner.addr);
        let mut out = Vec::new();
        for name in ["http_proxy", "https_proxy"] {
            out.push((name.to_string(), url.clone()));
            out.push((name.to_uppercase(), url.clone()));
        }
        // For what speaks SOCKS5 rather than HTTP, with the name left to the proxy.
        let socks = format!("socks5h://{}:x@{}", self.token, self.inner.addr);
        out.push(("all_proxy".into(), socks.clone()));
        out.push(("ALL_PROXY".into(), socks));
        out.push(("no_proxy".into(), LOCAL.into()));
        out.push(("NO_PROXY".into(), LOCAL.into()));
        // `git` over ssh is the one client here that reads no proxy variable.
        // A command line the person set is left alone.
        if std::env::var_os("GIT_SSH_COMMAND").is_none() {
            if let Some(command) = ssh_command() {
                out.push(("GIT_SSH_COMMAND".into(), command));
            }
        }
        out
    }
}

/// The `ssh` command line that has `git` reach its host through `lokode tunnel`.
///
/// None where the path of the executable would need quoting, so that the line never has to be
/// escaped.
/// On Linux the configuration files are skipped.
/// Files owned by root show as owned by nobody inside the namespace, and `ssh` refuses to read
/// them.
/// The sandbox hides the person's own `.ssh` directory as well.
fn ssh_command() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let exe = exe.to_str().filter(|p| plain(p))?;
    let mut line = String::from("ssh");
    #[cfg(target_os = "linux")]
    line.push_str(" -F /dev/null");
    line.push_str(&format!(" -o \"ProxyCommand={exe} tunnel %h %p\""));
    Some(line)
}

/// A path that a shell and `ssh` both read as one word.
fn plain(path: &str) -> bool {
    path.chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-+".contains(c))
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.inner.leases.lock().unwrap().remove(&self.token);
        self.session.done.cancel();
    }
}

impl Session {
    /// Note that a connection to `host` was turned away, for the command and for the journal.
    fn refused(&self, inner: &Inner, host: &str, why: &str) {
        inner.journal.refused(host, why);
        let mut all = self.turned_away.lock().unwrap();
        let line = format!("{host}: {why}");
        if !all.contains(&line) {
            all.push(line);
        }
    }

    /// Whether the command may reach `host`, asked once for as long as the lease lives.
    ///
    /// Connections that arrive together wait for the same answer.
    async fn verdict(&self, host: &str, port: u16) -> Answer {
        let cell = self
            .hosts
            .lock()
            .unwrap()
            .entry(crate::sandbox::approval::authority(host, port))
            .or_default()
            .clone();
        cell.get_or_init(|| async {
            let asked = self
                .gate
                .approve(ApprovalRequest {
                    tool: "network".into(),
                    summary: format!("{} connects to {host}:{port}", self.origin),
                    diff_preview: None,
                    scope: ApprovalScope::Host(crate::sandbox::approval::authority(host, port)),
                })
                .await;
            match asked {
                Approval::Proceed => Ok(()),
                Approval::Denied(why) => Err(denial(why)),
                Approval::Interrupted => Err("interrupted by user".to_string()),
            }
        })
        .await
        .clone()
    }
}

impl Session {
    /// Whether the command may go on sending to `host`, which the session has sent `sent` bytes
    /// to in all, when it may send `limit` before it is asked.
    async fn budget(&self, host: &str, sent: u64, limit: u64) -> Result<(), String> {
        let asked = self
            .gate
            .approve(ApprovalRequest {
                tool: "network".into(),
                summary: format!(
                    "{} has sent {} to {host} in this session, several times more than any \
                     earlier session (the most so far is {})",
                    self.origin,
                    size(sent),
                    size(limit / UNUSUAL_FACTOR)
                ),
                diff_preview: None,
                scope: ApprovalScope::Told(format!("larger uploads than usual to {host}")),
            })
            .await;
        match asked {
            Approval::Proceed => Ok(()),
            Approval::Denied(why) => Err(denial(why)),
            Approval::Interrupted => Err("interrupted by user".to_string()),
        }
    }

    /// Whether the command may go on sending to `host`, which it has sent `sent` bytes to.
    ///
    /// Asked when a connection sends several times more than any earlier one did.
    async fn volume(&self, host: &str, sent: u64, usual: u64) -> Result<(), String> {
        let summary = match usual {
            0 => format!(
                "{} has sent {} to {host}, and nothing is known yet of what is usual here",
                self.origin,
                size(sent)
            ),
            _ => format!(
                "{} has sent {} to {host}, several times more than any earlier connection to it \
                 (the most so far is {})",
                self.origin,
                size(sent),
                size(usual)
            ),
        };
        let asked = self
            .gate
            .approve(ApprovalRequest {
                tool: "network".into(),
                summary,
                diff_preview: None,
                scope: ApprovalScope::Told(format!("larger uploads than usual to {host}")),
            })
            .await;
        match asked {
            Approval::Proceed => Ok(()),
            Approval::Denied(why) => Err(denial(why)),
            Approval::Interrupted => Err("interrupted by user".to_string()),
        }
    }
}

/// The addresses of a host, and an error when one of them is not public.
///
/// The host is resolved here rather than trusted as written.
/// A name may point at a loopback or a metadata address.
pub async fn public_addrs(host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
    let found: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| format!("cannot resolve {host}: {e}"))?
        .collect();
    for addr in &found {
        if is_blocked(addr.ip()) {
            return Err(format!(
                "{host} resolves to {}, which is not a public address \
                 (pass --allow-host {host} if that is deliberate)",
                addr.ip()
            ));
        }
    }
    if found.is_empty() {
        return Err(format!("{host} resolves to nothing"));
    }
    Ok(found)
}

/// Whether an address belongs to a range that must not be reached.
///
/// Loopback and the link-local block that carries cloud metadata are the ones that matter most.
/// Every non-public range is refused.
/// Reaching a service inside the network the agent runs in is the same class of mistake.
pub fn is_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => is_blocked_v6(v6),
    }
}

fn is_blocked_v4(v4: Ipv4Addr) -> bool {
    let [a, b, ..] = v4.octets();
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_broadcast()
        || v4.is_documentation()
        || v4.is_unspecified()
        || v4.is_multicast()
        || a == 0
        // Carrier-grade NAT, 100.64.0.0/10.
        || (a == 100 && (b & 0xc0) == 64)
        // Benchmarking, 198.18.0.0/15, which the resolver of a namespace hands out.
        || (a == 198 && (b & 0xfe) == 18)
        // Reserved, 240.0.0.0/4.
        || a >= 240
}

fn is_blocked_v6(v6: Ipv6Addr) -> bool {
    if let Some(mapped) = v6.to_ipv4_mapped() {
        return is_blocked_v4(mapped);
    }
    let first = v6.segments()[0];
    v6.is_loopback()
        || v6.is_unspecified()
        || v6.is_multicast()
        // Unique local, fc00::/7.
        || (first & 0xfe00) == 0xfc00
        // Link-local unicast, fe80::/10.
        || (first & 0xffc0) == 0xfe80
}

/// The names that were handed out as addresses, and the address each was given.
///
/// The addresses come from a range that is not routed anywhere, one after the other.
/// When the range is used up, the oldest are given again.
#[derive(Default)]
struct Names {
    by_name: HashMap<String, Ipv4Addr>,
    by_address: HashMap<Ipv4Addr, String>,
    next: u32,
}

/// The first address handed out, 198.18.0.0, and how many the range holds, 198.18.0.0/15.
const HANDED_OUT_FIRST: u32 = 0xc612_0000;
const HANDED_OUT_COUNT: u32 = 1 << 17;

impl Names {
    /// The address that stands for `name`.
    fn address(&mut self, name: &str) -> Ipv4Addr {
        if let Some(address) = self.by_name.get(name) {
            return *address;
        }
        let address = Ipv4Addr::from(HANDED_OUT_FIRST + self.next % HANDED_OUT_COUNT);
        self.next = self.next.wrapping_add(1);
        if let Some(older) = self.by_address.insert(address, name.to_string()) {
            self.by_name.remove(&older);
        }
        self.by_name.insert(name.to_string(), address);
        address
    }

    /// The name an address was handed out for.
    fn name(&self, address: Ipv4Addr) -> Option<String> {
        self.by_address.get(&address).cloned()
    }
}

/// Whether an address belongs to the range the resolver hands out.
fn handed_out(address: Ipv4Addr) -> bool {
    let value = u32::from(address);
    (HANDED_OUT_FIRST..HANDED_OUT_FIRST + HANDED_OUT_COUNT).contains(&value)
}

/// The answer to a name lookup, or None where the message is not a query this resolver reads.
///
/// An address query gets an address of the range.
/// Every other query gets an empty answer, so that a client falls back to an address it was given.
/// The name is not looked up anywhere.
fn answer(query: &[u8], names: &mut Names) -> Option<Vec<u8>> {
    const HEADER: usize = 12;
    if query.len() < HEADER || query[2] & 0x80 != 0 || query[4..6] != [0, 1] {
        return None;
    }
    let mut at = HEADER;
    let mut name = String::new();
    loop {
        let length = *query.get(at)? as usize;
        at += 1;
        if length == 0 {
            break;
        }
        // A label is at most 63 bytes, and a longer first byte is a compression pointer.
        if length > 63 {
            return None;
        }
        let label = query.get(at..at + length)?;
        if !name.is_empty() {
            name.push('.');
        }
        name.push_str(&String::from_utf8_lossy(label).to_ascii_lowercase());
        at += length;
    }
    let kind = u16::from_be_bytes([*query.get(at)?, *query.get(at + 1)?]);
    let class = u16::from_be_bytes([*query.get(at + 2)?, *query.get(at + 3)?]);
    let question_end = at + 4;

    // Address query, internet class.
    let address = (kind == 1 && class == 1 && valid_host(&name)).then(|| names.address(&name));
    let mut out = Vec::with_capacity(question_end + 16);
    out.extend_from_slice(&query[..2]);
    // A response that the server recurses for, with no error.
    out.extend_from_slice(&[0x81, 0x80]);
    out.extend_from_slice(&[0, 1, 0, address.is_some() as u8, 0, 0, 0, 0]);
    out.extend_from_slice(&query[HEADER..question_end]);
    if let Some(address) = address {
        // The name again as a pointer, the type, the class, a lifetime of 0 and 4 bytes of address.
        out.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 0, 0, 4]);
        out.extend_from_slice(&address.octets());
    }
    Some(out)
}

/// Answer the name lookups of the namespace until the proxy is dropped.
async fn resolve(inner: Arc<Inner>, socket: tokio::net::UdpSocket) {
    let mut buf = [0u8; 512];
    loop {
        let received = tokio::select! {
            biased;
            _ = inner.stop.cancelled() => return,
            r = socket.recv_from(&mut buf) => r,
        };
        let Ok((n, from)) = received else {
            continue;
        };
        let reply = answer(&buf[..n], &mut inner.names.lock().unwrap());
        if let Some(reply) = reply {
            let _ = socket.send_to(&reply, from).await;
        }
    }
}

/// The address a redirected connection was made for, when it was redirected.
#[cfg(target_os = "linux")]
fn original_destination(client: &TcpStream) -> Option<SocketAddr> {
    use std::os::fd::AsRawFd;
    // Option 80 of the IP level, which the firewall keeps for the connections it redirects.
    const ORIGINAL_DST: libc::c_int = 80;
    // SAFETY: the pointers name a zeroed address and its size, both live for the call.
    unsafe {
        let mut addr: libc::sockaddr_in = std::mem::zeroed();
        let mut size = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
        let rc = libc::getsockopt(
            client.as_raw_fd(),
            libc::SOL_IP,
            ORIGINAL_DST,
            (&mut addr as *mut libc::sockaddr_in).cast(),
            &mut size,
        );
        (rc == 0).then(|| {
            SocketAddr::new(
                IpAddr::V4(Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr))),
                u16::from_be(addr.sin_port),
            )
        })
    }
}

#[cfg(not(target_os = "linux"))]
fn original_destination(_client: &TcpStream) -> Option<SocketAddr> {
    None
}

/// A connection that was redirected here.
///
/// It carries no lease, so it is put to the command that holds the only one.
/// With none, nobody asked for the network, and with several, the command that opened it cannot
/// be told: the connection is closed.
async fn redirected(inner: Arc<Inner>, client: TcpStream, to: SocketAddr) {
    let host = match to.ip() {
        IpAddr::V4(address) if handed_out(address) => {
            match inner.names.lock().unwrap().name(address) {
                Some(name) => name,
                None => return,
            }
        }
        ip => ip.to_string(),
    };
    let session = {
        let leases = inner.leases.lock().unwrap();
        match leases.len() {
            1 => leases.values().next().cloned(),
            _ => None,
        }
    };
    let Some(session) = session else {
        return;
    };
    let request = Request {
        kind: Kind::Raw,
        host,
        port: to.port(),
        token: None,
        forward: Vec::new(),
    };
    serve(&inner, &session, client, request, Vec::new()).await;
}

/// Whether `host` is a name or an address the proxy may be asked about.
///
/// A name is made of letters, digits, hyphens and underscores in labels.
/// Anything else, such as a wildcard, a space or a line break, would be written into questions
/// and into the list of agreed hosts as it came.
/// A last label made of digits, or a hexadecimal one, is a number that a resolver reads as an
/// address, so it is refused unless it is an address in its usual form.
pub(crate) fn valid_host(host: &str) -> bool {
    if host.parse::<IpAddr>().is_ok() {
        return true;
    }
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    let labels: Vec<&str> = host.split('.').collect();
    let sound = labels.iter().all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    });
    let last = labels[labels.len() - 1];
    sound
        && !last.bytes().all(|b| b.is_ascii_digit())
        && !last.to_ascii_lowercase().starts_with("0x")
}

/// How a request reaches its host.
#[derive(Debug, PartialEq, Eq)]
enum Kind {
    /// A tunnel, which carries whatever the two ends say.
    Connect,
    /// One request in clear, rewritten for the origin.
    Plain,
    /// A SOCKS5 connection, which answers in bytes rather than in a status line.
    Socks,
    /// A connection that was redirected here, which speaks to the host and not to the proxy.
    Raw,
}

/// A request head, read and understood.
#[derive(Debug)]
struct Request {
    kind: Kind,
    host: String,
    port: u16,
    /// The token the command gave as the proxy user name.
    token: Option<String>,
    /// What goes to the host first: the rewritten head of a plain request, nothing for a tunnel.
    forward: Vec<u8>,
}

/// Read the request line and headers.
///
/// Returns the head and the bytes that arrived after it.
async fn read_head(client: &mut TcpStream) -> Result<(Vec<u8>, Vec<u8>), &'static str> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let rest = buf.split_off(end + 4);
            return Ok((buf, rest));
        }
        if buf.len() > MAX_HEAD_BYTES {
            return Err("request head too large");
        }
        match client.read(&mut chunk).await {
            Ok(0) | Err(_) => return Err("connection closed before the request was complete"),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

/// The host and port of a tunnel target or an authority, in normal form.
///
/// Lower case, no trailing dot, no brackets around an IPv6 address.
fn authority(raw: &str, default_port: Option<u16>) -> Result<(String, u16), &'static str> {
    let (host, port) = match raw.rsplit_once(':') {
        Some((h, p)) if !h.ends_with(':') || h.starts_with('[') => {
            (h, p.parse::<u16>().map_err(|_| "unusable port")?)
        }
        _ => (raw, default_port.ok_or("the target has no port")?),
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return Err("the target has no host");
    }
    Ok((host, port))
}

/// Understand a head.
///
/// A plain request leaves rewritten for the origin: the path alone on the request line,
/// no proxy header, and a connection that closes after the answer.
fn parse(head: &[u8]) -> Result<Request, &'static str> {
    let text = std::str::from_utf8(head).map_err(|_| "the request is not text")?;
    let mut lines = text.split("\r\n");
    let first = lines.next().ok_or("empty request")?;
    let mut parts = first.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err("malformed request line");
    };

    let mut token = None;
    let mut kept = Vec::new();
    for line in lines.filter(|l| !l.is_empty()) {
        let Some((name, value)) = line.split_once(':') else {
            return Err("malformed header");
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "proxy-authorization" => token = basic_user(value.trim()),
            // The host is the one of the target, which the proxy asked about.
            "proxy-connection" | "connection" | "host" => {}
            _ => kept.push(line),
        }
    }

    if method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = authority(target, None)?;
        return Ok(Request {
            kind: Kind::Connect,
            host,
            port,
            token,
            forward: Vec::new(),
        });
    }

    let url = url::Url::parse(target).map_err(|_| "the target is not an absolute URL")?;
    if url.scheme() != "http" {
        return Err("only http targets travel in clear");
    }
    let host = url.host_str().ok_or("the target has no host")?;
    let (host, port) = authority(
        &match url.port() {
            Some(p) => format!("{host}:{p}"),
            None => host.to_string(),
        },
        Some(80),
    )?;
    let mut origin_form = url.path().to_string();
    if let Some(q) = url.query() {
        origin_form.push('?');
        origin_form.push_str(q);
    }
    let mut forward = format!("{method} {origin_form} {version}\r\n").into_bytes();
    let named = match url.port() {
        Some(p) => format!("{}:{p}", url.host_str().unwrap_or_default()),
        None => url.host_str().unwrap_or_default().to_string(),
    };
    forward.extend_from_slice(format!("Host: {named}\r\n").as_bytes());
    for line in kept {
        forward.extend_from_slice(line.as_bytes());
        forward.extend_from_slice(b"\r\n");
    }
    forward.extend_from_slice(b"Connection: close\r\n\r\n");
    Ok(Request {
        kind: Kind::Plain,
        host,
        port,
        token,
        forward,
    })
}

/// The user name of a `Basic` credential.
fn basic_user(value: &str) -> Option<String> {
    let encoded = value.strip_prefix("Basic ")?.trim();
    let raw = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    let text = String::from_utf8(raw).ok()?;
    Some(text.split(':').next()?.to_string())
}

/// Answer a request with a refusal the command can show.
async fn reply(client: &mut TcpStream, status: &str, reason: &str, extra: &str) {
    let body = format!("lokode: {reason}\n");
    let text = format!(
        "HTTP/1.1 {status}\r\n{extra}Content-Type: text/plain\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = client.write_all(text.as_bytes()).await;
    let _ = client.shutdown().await;
}

/// One connection from a command.
async fn connection(inner: Arc<Inner>, mut client: TcpStream) {
    let mut first = [0u8; 1];
    match tokio::time::timeout(HEAD_TIMEOUT, client.peek(&mut first)).await {
        Ok(Ok(1)) if first[0] == SOCKS_VERSION => return socks(inner, client).await,
        Ok(_) => {}
        Err(_) => return,
    }
    let (head, rest) = match tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut client)).await {
        Ok(Ok(h)) => h,
        Ok(Err(why)) => return reply(&mut client, "400 Bad Request", why, "").await,
        Err(_) => return,
    };
    let request = match parse(&head) {
        Ok(r) => r,
        Err(why) => return reply(&mut client, "400 Bad Request", why, "").await,
    };
    let session = request
        .token
        .as_ref()
        .and_then(|t| inner.leases.lock().unwrap().get(t).cloned());
    let Some(session) = session else {
        return reply(
            &mut client,
            "407 Proxy Authentication Required",
            "this command has no right to the network",
            "Proxy-Authenticate: Basic realm=\"lokode\"\r\n",
        )
        .await;
    };
    serve(&inner, &session, client, request, rest).await;
}

/// Carry a request for as long as the proxy and the lease live.
async fn serve(
    inner: &Inner,
    session: &Session,
    client: TcpStream,
    request: Request,
    rest: Vec<u8>,
) {
    tokio::select! {
        biased;
        _ = inner.stop.cancelled() => {}
        _ = session.done.cancelled() => {}
        _ = relay(inner, session, client, request, rest) => {}
    }
}

/// Turn a connection away in the words of its protocol.
async fn refuse(client: &mut TcpStream, kind: &Kind, status: u16, why: &str) {
    match kind {
        Kind::Socks => {
            // 2 is "not allowed by the ruleset", 4 is "host unreachable".
            let code = if status == 502 { 4 } else { 2 };
            socks_answer(client, code).await;
        }
        // There is no one to tell: the client only sees the connection close.
        Kind::Raw => {
            let _ = client.shutdown().await;
        }
        _ if status == 502 => reply(client, "502 Bad Gateway", why, "").await,
        _ => reply(client, "403 Forbidden", why, "").await,
    }
}

/// The version byte that opens a SOCKS5 exchange.
const SOCKS_VERSION: u8 = 5;

/// The answer to a granted SOCKS5 request, with no address to report.
const SOCKS_GRANTED: [u8; 10] = [SOCKS_VERSION, 0, 0, 1, 0, 0, 0, 0, 0, 0];

/// Answer a SOCKS5 request with `code` and hang up.
async fn socks_answer(client: &mut TcpStream, code: u8) {
    let _ = client
        .write_all(&[SOCKS_VERSION, code, 0, 1, 0, 0, 0, 0, 0, 0])
        .await;
    let _ = client.shutdown().await;
}

/// One SOCKS5 connection: the credentials of a lease, then a request to connect.
async fn socks(inner: Arc<Inner>, mut client: TcpStream) {
    let negotiated = tokio::time::timeout(HEAD_TIMEOUT, socks_negotiate(&inner, &mut client)).await;
    let Ok(Ok(Some((session, host, port)))) = negotiated else {
        return;
    };
    let request = Request {
        kind: Kind::Socks,
        host,
        port,
        token: None,
        forward: Vec::new(),
    };
    serve(&inner, &session, client, request, Vec::new()).await;
}

/// The lease and the target a SOCKS5 client asks for.
///
/// `None` when the exchange ends without a request to carry.
async fn socks_negotiate(
    inner: &Inner,
    client: &mut TcpStream,
) -> std::io::Result<Option<(Arc<Session>, String, u16)>> {
    // Only the user name and password method is offered, since that is what carries the lease.
    const PASSWORD: u8 = 2;
    let mut greeting = [0u8; 2];
    client.read_exact(&mut greeting).await?;
    let mut methods = vec![0u8; greeting[1] as usize];
    client.read_exact(&mut methods).await?;
    if !methods.contains(&PASSWORD) {
        client.write_all(&[SOCKS_VERSION, 0xff]).await?;
        return Ok(None);
    }
    client.write_all(&[SOCKS_VERSION, PASSWORD]).await?;

    let mut auth = [0u8; 2];
    client.read_exact(&mut auth).await?;
    let mut user = vec![0u8; auth[1] as usize];
    client.read_exact(&mut user).await?;
    let mut length = [0u8; 1];
    client.read_exact(&mut length).await?;
    let mut password = vec![0u8; length[0] as usize];
    client.read_exact(&mut password).await?;
    let token = String::from_utf8_lossy(&user).into_owned();
    let session = inner.leases.lock().unwrap().get(&token).cloned();
    let Some(session) = session else {
        client.write_all(&[1, 1]).await?;
        return Ok(None);
    };
    client.write_all(&[1, 0]).await?;

    let mut request = [0u8; 4];
    client.read_exact(&mut request).await?;
    // 1 is connect, the only command carried.
    if request[0] != SOCKS_VERSION || request[1] != 1 {
        socks_answer(client, 7).await;
        return Ok(None);
    }
    let host = match request[3] {
        1 => {
            let mut a = [0u8; 4];
            client.read_exact(&mut a).await?;
            Ipv4Addr::from(a).to_string()
        }
        4 => {
            let mut a = [0u8; 16];
            client.read_exact(&mut a).await?;
            Ipv6Addr::from(a).to_string()
        }
        3 => {
            let mut n = [0u8; 1];
            client.read_exact(&mut n).await?;
            let mut name = vec![0u8; n[0] as usize];
            client.read_exact(&mut name).await?;
            let name = String::from_utf8_lossy(&name);
            name.trim_end_matches('.').to_ascii_lowercase()
        }
        _ => {
            socks_answer(client, 8).await;
            return Ok(None);
        }
    };
    let mut port = [0u8; 2];
    client.read_exact(&mut port).await?;
    if host.is_empty() {
        socks_answer(client, 1).await;
        return Ok(None);
    }
    Ok(Some((session, host, u16::from_be_bytes(port))))
}

/// Decide, connect and carry the bytes both ways.
async fn relay(
    inner: &Inner,
    session: &Session,
    mut client: TcpStream,
    request: Request,
    rest: Vec<u8>,
) {
    let Request {
        kind,
        host,
        port,
        forward,
        ..
    } = request;
    let reachable = inner.allow_hosts.contains(&host);

    // Nothing is asked, recorded or journaled for a host that is not one.
    if !valid_host(&host) {
        tracing::info!(host = ?host, "connection refused: not a host name");
        return refuse(&mut client, &kind, 403, "that is not a host name").await;
    }

    inner.journal.reached(&host, port);

    // A literal address needs no lookup, so it is refused before anyone is asked.
    if !reachable {
        if let Ok(ip) = host.parse::<IpAddr>() {
            if is_blocked(ip) {
                let why = format!(
                    "{host} is not a public address \
                     (pass --allow-host {host} if that is deliberate)"
                );
                session.refused(inner, &host, &why);
                return refuse(&mut client, &kind, 403, &why).await;
            }
        }
    }

    // A name is looked up only once it has been agreed to.
    if let Err(why) = session.verdict(&host, port).await {
        tracing::info!(origin = %session.origin, %host, port, %why, "connection refused");
        session.refused(inner, &host, &why);
        return refuse(&mut client, &kind, 403, &why).await;
    }
    let addrs = if reachable {
        match tokio::net::lookup_host((host.as_str(), port)).await {
            Ok(a) => Ok(a.collect::<Vec<_>>()),
            Err(e) => Err(format!("cannot resolve {host}: {e}")),
        }
    } else {
        public_addrs(&host, port).await
    };
    let addrs = match addrs {
        Ok(a) => a,
        Err(why) => {
            tracing::info!(origin = %session.origin, %host, port, %why, "connection refused");
            session.refused(inner, &host, &why);
            return refuse(&mut client, &kind, 403, &why).await;
        }
    };

    // The address that was checked is the address connected to.
    let mut upstream = None;
    for addr in addrs {
        if let Ok(s) = TcpStream::connect(addr).await {
            upstream = Some(s);
            break;
        }
    }
    let Some(mut upstream) = upstream else {
        let why = format!("cannot connect to {host}:{port}");
        session.refused(inner, &host, &why);
        return refuse(&mut client, &kind, 502, &why).await;
    };

    let opened = match kind {
        Kind::Connect => {
            let ok = b"HTTP/1.1 200 Connection Established\r\n\r\n";
            client.write_all(ok).await.is_ok() && upstream.write_all(&rest).await.is_ok()
        }
        Kind::Plain => {
            upstream.write_all(&forward).await.is_ok() && upstream.write_all(&rest).await.is_ok()
        }
        Kind::Socks => client.write_all(&SOCKS_GRANTED).await.is_ok(),
        Kind::Raw => true,
    };
    if !opened {
        return;
    }
    // Before anything is known of the project, the first upload to a host is put to the person.
    let usual = Some(inner.journal.usual_for(&host).unwrap_or_default());
    let budget = inner.journal.session_budget(&host);
    let watch = Some(Watch {
        usual,
        budget,
        session,
        host: &host,
    });
    let (sent, received, stopped) = carry(&mut client, &mut upstream, watch).await;
    match stopped {
        Some(why) => {
            tracing::info!(origin = %session.origin, %host, port, sent, %why, "connection stopped");
            session.refused(inner, &host, &why);
        }
        None => {
            tracing::info!(
                origin = %session.origin, %host, port, sent, received,
                "connection closed"
            );
            inner.journal.connected(&host, sent, received);
        }
    }
}

/// What a connection is measured against.
struct Watch<'a> {
    usual: Option<Usual>,
    /// What the session has sent to the host already, and the most it may send.
    budget: Option<(u64, u64)>,
    session: &'a Session,
    host: &'a str,
}

/// How many times larger than the largest earlier upload to its host a connection must be to be
/// put to the person.
const UNUSUAL_FACTOR: u64 = 4;

/// The bytes read in one go.
const COPY_BUFFER: usize = 16 * 1024;

/// Carry the bytes both ways until both sides have finished.
///
/// Returns the bytes sent and received, and why the connection was stopped when it was.
/// The first upload to exceed what is usual for the host waits for an answer, and is cut off
/// if the answer is no.
async fn carry(
    client: &mut TcpStream,
    upstream: &mut TcpStream,
    watch: Option<Watch<'_>>,
) -> (u64, u64, Option<String>) {
    let sent = AtomicU64::new(0);
    let received = AtomicU64::new(0);
    let (mut from_client, mut to_client) = client.split();
    let (mut from_host, mut to_host) = upstream.split();
    let up = async {
        let mut buf = vec![0u8; COPY_BUFFER];
        let mut asked = false;
        let mut over_budget = false;
        loop {
            let n = from_client
                .read(&mut buf)
                .await
                .map_err(|e| e.to_string())?;
            if n == 0 {
                let _ = to_host.shutdown().await;
                return Ok(());
            }
            let total = sent.load(Ordering::Relaxed) + n as u64;
            if let (false, Some(w)) = (asked, &watch) {
                if let Some(usual) = w.usual {
                    let got = received.load(Ordering::Relaxed);
                    // Until the answer has grown to the size of the largest earlier upload, the
                    // proportion of what came back says nothing.
                    if total > usual.sent.saturating_mul(UNUSUAL_FACTOR)
                        && (got < usual.sent || ratio(total, got) > usual.ratio)
                    {
                        asked = true;
                        w.session.volume(w.host, total, usual.sent).await?;
                    }
                }
            }
            // An upload split over many connections adds up.
            if let (false, Some(w)) = (over_budget, &watch) {
                if let Some((already, limit)) = w.budget {
                    if already + total > limit {
                        over_budget = true;
                        w.session.budget(w.host, already + total, limit).await?;
                    }
                }
            }
            to_host
                .write_all(&buf[..n])
                .await
                .map_err(|e| e.to_string())?;
            sent.fetch_add(n as u64, Ordering::Relaxed);
        }
    };
    let down = async {
        let mut buf = vec![0u8; COPY_BUFFER];
        loop {
            let n = from_host.read(&mut buf).await.map_err(|e| e.to_string())?;
            if n == 0 {
                let _ = to_client.shutdown().await;
                return Ok(());
            }
            to_client
                .write_all(&buf[..n])
                .await
                .map_err(|e| e.to_string())?;
            received.fetch_add(n as u64, Ordering::Relaxed);
        }
    };
    let ended: Result<((), ()), String> = tokio::try_join!(up, down);
    let stopped = ended.err();
    (
        sent.load(Ordering::Relaxed),
        received.load(Ordering::Relaxed),
        stopped,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use crate::sandbox::approval::Denial;

    fn v4(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    /// The address a compromised fetch is aimed at first.
    #[test]
    fn cloud_metadata_is_blocked() {
        assert!(is_blocked(v4("169.254.169.254")));
    }

    #[test]
    fn loopback_and_private_ranges_are_blocked() {
        for addr in [
            "127.0.0.1",
            "10.1.2.3",
            "192.0.2.1",
            "172.16.0.1",
            "0.0.0.0",
        ] {
            assert!(is_blocked(v4(addr)), "{addr} should be refused");
        }
    }

    #[test]
    fn carrier_grade_nat_is_blocked() {
        assert!(is_blocked(v4("100.64.0.1")));
        assert!(is_blocked(v4("100.127.255.255")));
        // 100.63 and 100.128 are outside the /10 and are ordinary public space.
        assert!(!is_blocked(v4("100.63.255.255")));
        assert!(!is_blocked(v4("100.128.0.1")));
    }

    #[test]
    fn public_addresses_are_allowed() {
        for addr in ["1.1.1.1", "8.8.8.8", "93.184.216.34"] {
            assert!(!is_blocked(v4(addr)), "{addr} should be allowed");
        }
    }

    /// An IPv4 address wearing an IPv6 costume is the same address.
    #[test]
    fn ipv4_mapped_addresses_are_judged_as_ipv4() {
        assert!(is_blocked(v4("::ffff:127.0.0.1")));
        assert!(is_blocked(v4("::ffff:169.254.169.254")));
        assert!(!is_blocked(v4("::ffff:8.8.8.8")));
    }

    #[test]
    fn ipv6_local_ranges_are_blocked() {
        for addr in ["::1", "::", "fc00::1", "fd12:3456::1", "fe80::1"] {
            assert!(is_blocked(v4(addr)), "{addr} should be refused");
        }
        assert!(!is_blocked(v4("2606:4700:4700::1111")));
    }

    /// A gate that answers the same way every time and counts the questions.
    struct Fixed {
        answer: fn() -> Approval,
        asked: AtomicUsize,
        scopes: Mutex<Vec<ApprovalScope>>,
        summaries: Mutex<Vec<String>>,
        delay: Duration,
    }

    impl Fixed {
        fn new(answer: fn() -> Approval) -> Arc<Self> {
            Arc::new(Self {
                answer,
                asked: AtomicUsize::new(0),
                scopes: Mutex::new(Vec::new()),
                summaries: Mutex::new(Vec::new()),
                delay: Duration::ZERO,
            })
        }

        fn slow(answer: fn() -> Approval, delay: Duration) -> Arc<Self> {
            Arc::new(Self {
                delay,
                ..Arc::try_unwrap(Self::new(answer)).ok().unwrap()
            })
        }

        fn asked(&self) -> usize {
            self.asked.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Gate for Fixed {
        async fn approve(&self, req: ApprovalRequest) -> Approval {
            self.asked.fetch_add(1, Ordering::SeqCst);
            self.scopes.lock().unwrap().push(req.scope);
            self.summaries.lock().unwrap().push(req.summary);
            tokio::time::sleep(self.delay).await;
            (self.answer)()
        }
    }

    fn yes() -> Approval {
        Approval::Proceed
    }

    fn no() -> Approval {
        Approval::Denied(Denial::User(None))
    }

    fn by_rule() -> Approval {
        Approval::Denied(Denial::Rule("refused by the policy rule \"*.evil\"".into()))
    }

    /// A journal that has seen uploads far larger than any test sends, for the loopback host.
    fn roomy_journal() -> Journal {
        let journal = Journal::default();
        journal.usual.lock().unwrap().insert(
            "127.0.0.1".into(),
            Usual {
                sent: 1 << 30,
                ratio: 1e9,
                session: 0,
            },
        );
        journal
    }

    /// A proxy that reaches loopback, with a lease decided by `gate`.
    async fn proxy_with(gate: Arc<Fixed>) -> (Proxy, Lease) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let allow = Arc::new(["127.0.0.1".to_string()].into_iter().collect());
        let proxy = Proxy::serve(listener, allow, Arc::new(roomy_journal())).unwrap();
        let lease = proxy.lease(gate, "bash: curl");
        (proxy, lease)
    }

    /// The credential line a command sends.
    fn credential(lease: &Lease) -> String {
        let user = format!("{}:x", lease.token);
        let b64 = base64::engine::general_purpose::STANDARD.encode(user);
        format!("Proxy-Authorization: Basic {b64}\r\n")
    }

    /// What the proxy answers to `request`, read until it hangs up.
    async fn exchange(proxy: &Proxy, request: &str) -> String {
        let mut s = TcpStream::connect(proxy.local_addr()).await.unwrap();
        s.write_all(request.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out).await;
        String::from_utf8_lossy(&out).into_owned()
    }

    /// A host that echoes what it reads, and counts how many connected.
    async fn echo() -> (u16, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                seen.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    while let Ok(n) = sock.read(&mut buf).await {
                        if n == 0 || sock.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        (port, count)
    }

    /// An agreed tunnel carries bytes both ways.
    #[tokio::test]
    async fn an_agreed_tunnel_carries_bytes() {
        let (port, _) = echo().await;
        let gate = Fixed::new(yes);
        let (proxy, lease) = proxy_with(gate.clone()).await;

        let mut s = TcpStream::connect(proxy.local_addr()).await.unwrap();
        let req = format!(
            "CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{}\r\n",
            credential(&lease)
        );
        s.write_all(req.as_bytes()).await.unwrap();
        let mut head = vec![0u8; 39];
        s.read_exact(&mut head).await.unwrap();
        assert!(String::from_utf8_lossy(&head).starts_with("HTTP/1.1 200"));

        s.write_all(b"ping").await.unwrap();
        let mut back = [0u8; 4];
        s.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"ping");

        assert_eq!(gate.asked(), 1);
        assert_eq!(
            gate.scopes.lock().unwrap()[0],
            ApprovalScope::Host(format!("127.0.0.1:{port}"))
        );
        let said = gate.summaries.lock().unwrap()[0].clone();
        assert!(said.contains("bash: curl") && said.contains(&format!("127.0.0.1:{port}")));
    }

    /// A host the gate refuses is never connected to, and the command is told why.
    #[tokio::test]
    async fn a_refused_host_is_not_connected_to() {
        let (port, connected) = echo().await;
        let (proxy, lease) = proxy_with(Fixed::new(no)).await;
        let req = format!(
            "CONNECT 127.0.0.1:{port} HTTP/1.1\r\n{}\r\n",
            credential(&lease)
        );
        let answer = exchange(&proxy, &req).await;
        assert!(answer.starts_with("HTTP/1.1 403"), "{answer}");
        assert!(answer.contains("user denied"), "{answer}");
        assert_eq!(connected.load(Ordering::SeqCst), 0);
    }

    /// A rule of the configuration is named in what the command reads.
    #[tokio::test]
    async fn a_rule_is_named_in_the_refusal() {
        let (port, _) = echo().await;
        let (proxy, lease) = proxy_with(Fixed::new(by_rule)).await;
        let req = format!(
            "CONNECT 127.0.0.1:{port} HTTP/1.1\r\n{}\r\n",
            credential(&lease)
        );
        let answer = exchange(&proxy, &req).await;
        assert!(answer.contains("policy rule"), "{answer}");
    }

    /// Without the token of a lease nothing goes through, and nobody is asked.
    #[tokio::test]
    async fn a_request_without_a_lease_is_refused_unasked() {
        let gate = Fixed::new(yes);
        let (proxy, lease) = proxy_with(gate.clone()).await;

        let bare = exchange(&proxy, "CONNECT 127.0.0.1:9 HTTP/1.1\r\n\r\n").await;
        assert!(bare.starts_with("HTTP/1.1 407"), "{bare}");

        let wrong = "Proxy-Authorization: Basic eDp4\r\n";
        let forged = exchange(
            &proxy,
            &format!("CONNECT 127.0.0.1:9 HTTP/1.1\r\n{wrong}\r\n"),
        )
        .await;
        assert!(forged.starts_with("HTTP/1.1 407"), "{forged}");

        let line = credential(&lease);
        drop(lease);
        let ended = exchange(
            &proxy,
            &format!("CONNECT 127.0.0.1:9 HTTP/1.1\r\n{line}\r\n"),
        )
        .await;
        assert!(ended.starts_with("HTTP/1.1 407"), "{ended}");
        assert_eq!(gate.asked(), 0);
    }

    /// Connections that arrive together to one host share a single question.
    #[tokio::test]
    async fn one_host_is_asked_once_however_many_connections() {
        let (port, _) = echo().await;
        let gate = Fixed::slow(yes, Duration::from_millis(100));
        let (proxy, lease) = proxy_with(gate.clone()).await;
        let req = format!(
            "CONNECT 127.0.0.1:{port} HTTP/1.1\r\n{}\r\n",
            credential(&lease)
        );
        let mut open = Vec::new();
        for _ in 0..4 {
            let mut s = TcpStream::connect(proxy.local_addr()).await.unwrap();
            s.write_all(req.as_bytes()).await.unwrap();
            open.push(s);
        }
        for s in &mut open {
            let mut head = vec![0u8; 39];
            s.read_exact(&mut head).await.unwrap();
            assert!(String::from_utf8_lossy(&head).starts_with("HTTP/1.1 200"));
        }
        assert_eq!(gate.asked(), 1);
    }

    /// A private address given as a literal is refused before anyone is asked.
    #[tokio::test]
    async fn a_private_literal_is_refused_unasked() {
        let gate = Fixed::new(yes);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = Proxy::serve(
            listener,
            Arc::new(HashSet::new()),
            Arc::new(Journal::default()),
        )
        .unwrap();
        let lease = proxy.lease(gate.clone(), "bash: curl");
        for target in ["169.254.169.254:80", "127.0.0.1:80", "[::1]:80"] {
            let req = format!("CONNECT {target} HTTP/1.1\r\n{}\r\n", credential(&lease));
            let answer = exchange(&proxy, &req).await;
            assert!(answer.starts_with("HTTP/1.1 403"), "{target}: {answer}");
            assert!(answer.contains("not a public address"), "{answer}");
        }
        assert_eq!(gate.asked(), 0);
    }

    /// A name that is refused is never looked up.
    /// The refusal reads as the gate's, which a failed lookup would not.
    #[tokio::test]
    async fn a_name_is_agreed_to_before_it_is_looked_up() {
        let gate = Fixed::new(no);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = Proxy::serve(
            listener,
            Arc::new(HashSet::new()),
            Arc::new(Journal::default()),
        )
        .unwrap();
        let lease = proxy.lease(gate.clone(), "bash: curl");
        let req = format!(
            "CONNECT secret-bytes.unresolvable.invalid:443 HTTP/1.1\r\n{}\r\n",
            credential(&lease)
        );
        let answer = exchange(&proxy, &req).await;
        assert!(answer.contains("user denied"), "{answer}");
        assert!(!answer.contains("cannot resolve"), "{answer}");
        assert_eq!(gate.asked(), 1);
    }

    /// A plain request reaches the host as its own request, with nothing of the proxy in it.
    #[tokio::test]
    async fn a_plain_request_is_rewritten_for_its_host() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let received = Arc::new(Mutex::new(String::new()));
        let kept = received.clone();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = sock.read(&mut buf).await.unwrap();
            *kept.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).into_owned();
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .await
                .unwrap();
        });

        let (proxy, lease) = proxy_with(Fixed::new(yes)).await;
        let req = format!(
            "GET http://127.0.0.1:{port}/a/b?c=1 HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
             {}Proxy-Connection: keep-alive\r\nAccept: */*\r\n\r\n",
            credential(&lease)
        );
        let answer = exchange(&proxy, &req).await;
        assert!(
            answer.starts_with("HTTP/1.1 200") && answer.ends_with("ok"),
            "{answer}"
        );

        let seen = received.lock().unwrap().clone();
        assert!(seen.starts_with("GET /a/b?c=1 HTTP/1.1\r\n"), "{seen}");
        assert!(seen.contains("Accept: */*"), "{seen}");
        assert!(seen.contains("Connection: close"), "{seen}");
        assert!(!seen.to_lowercase().contains("proxy-"), "{seen}");
    }

    /// The host header is the one of the target that was asked about, whatever the client said.
    #[tokio::test]
    async fn a_plain_request_is_sent_the_host_that_was_asked_about() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let received = Arc::new(Mutex::new(String::new()));
        let kept = received.clone();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = sock.read(&mut buf).await.unwrap();
            *kept.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).into_owned();
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await;
        });
        let (proxy, lease) = proxy_with(Fixed::new(yes)).await;
        let req = format!(
            "GET http://127.0.0.1:{port}/ HTTP/1.1\r\nHost: other.example\r\n{}\r\n",
            credential(&lease)
        );
        exchange(&proxy, &req).await;
        let seen = received.lock().unwrap().clone();
        assert!(
            seen.contains(&format!("Host: 127.0.0.1:{port}\r\n")),
            "{seen}"
        );
        assert!(!seen.contains("other.example"), "{seen}");
    }

    /// A connection that never says what it wants is closed, and holds nothing open.
    #[tokio::test]
    async fn a_connection_that_sends_nothing_is_closed() {
        let (proxy, _lease) = proxy_with(Fixed::new(yes)).await;
        let mut silent = TcpStream::connect(proxy.local_addr()).await.unwrap();
        let mut buf = [0u8; 16];
        let read = tokio::time::timeout(Duration::from_secs(5), silent.read(&mut buf)).await;
        assert!(matches!(read, Ok(Ok(0))), "{read:?}");
    }

    /// With 2 commands running, a connection that carries no lease cannot be put to either.
    #[tokio::test]
    async fn a_redirected_connection_is_closed_when_2_commands_hold_the_network() {
        let gate = Fixed::new(yes);
        let (proxy, _first) = proxy_with(gate.clone()).await;
        let _second = proxy.lease(gate.clone(), "bash: other");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (served, _) = listener.accept().await.unwrap();
        let to = "93.184.216.34:443".parse().unwrap();
        redirected(proxy.inner.clone(), served, to).await;
        let mut buf = [0u8; 4];
        assert!(matches!(client.read(&mut buf).await, Ok(0)));
        assert_eq!(gate.asked(), 0);
    }

    #[test]
    fn only_a_name_or_an_address_is_a_host() {
        for good in [
            "example.com",
            "a-b.example.co.uk",
            "xn--bcher-kva.example",
            "localhost",
            "under_score.example",
            "1.2.3.4",
            "::1",
            "2606:4700::1111",
        ] {
            assert!(valid_host(good), "{good}");
        }
        for bad in [
            "",
            "*",
            "docs.rs*",
            "*.example.com",
            "a b.example",
            "a\nb.example",
            "a`b.example",
            "-a.example",
            "a..example",
            "16909060",
            "0x7f.1",
            "1.2.3",
            "fe80::1%eth0",
        ] {
            assert!(!valid_host(bad), "{bad:?}");
        }
    }

    /// A target that is not a host is refused before anyone is asked or anything is recorded.
    #[tokio::test]
    async fn a_target_that_is_not_a_host_is_refused_unasked() {
        let gate = Fixed::new(yes);
        let (proxy, lease) = proxy_with(gate.clone()).await;
        for target in ["*:443", "docs.rs*:443", "16909060:443"] {
            let req = format!("CONNECT {target} HTTP/1.1\r\n{}\r\n", credential(&lease));
            let answer = exchange(&proxy, &req).await;
            assert!(answer.starts_with("HTTP/1.1 403"), "{target}: {answer}");
        }
        assert_eq!(gate.asked(), 0);
        assert!(proxy.inner.journal.seen().is_empty());
    }

    /// A name that is not a host gets an empty answer and no address.
    #[test]
    fn a_lookup_for_something_that_is_not_a_host_is_given_no_address() {
        let mut names = Names::default();
        let reply = answer(&query("*", 1), &mut names).unwrap();
        assert_eq!(&reply[6..8], &[0, 0]);
        assert!(names.by_name.is_empty());
    }

    /// A history file that cannot be read is left as it is.
    #[test]
    fn a_history_file_that_cannot_be_read_is_not_overwritten() {
        let data = tempfile::tempdir().unwrap();
        crate::paths::with_env(&[("XDG_DATA_HOME", data.path().to_str())], || {
            let path = usual_path().unwrap();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "{ not json").unwrap();
            Journal::kept_for(Path::new("/somewhere/project")).connected("a.test", 700, 70);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
        });
    }

    /// The command is given a proxy address that carries its lease, and no exception.
    #[tokio::test]
    async fn the_environment_names_the_proxy_and_only_local_exceptions() {
        let (proxy, lease) = proxy_with(Fixed::new(yes)).await;
        let env: HashMap<String, String> = lease.env().into_iter().collect();
        let url = format!("http://{}:x@{}", lease.token, proxy.local_addr());
        for name in ["HTTP_PROXY", "https_proxy"] {
            assert_eq!(env[name], url, "{name}");
        }
        let socks = format!("socks5h://{}:x@{}", lease.token, proxy.local_addr());
        assert_eq!(env["ALL_PROXY"], socks);
        assert_eq!(env["NO_PROXY"], LOCAL);
        assert_eq!(env["no_proxy"], LOCAL);
    }

    /// `git` over ssh is given a line that runs the tunnel, as one word per option.
    #[test]
    fn git_over_ssh_is_pointed_at_the_tunnel() {
        let Some(line) = ssh_command() else {
            return;
        };
        assert!(line.starts_with("ssh "), "{line}");
        assert!(line.ends_with(" tunnel %h %p\""), "{line}");
        assert!(!line.contains('\''), "{line}");
    }

    #[test]
    fn only_a_plain_path_goes_into_a_command_line() {
        assert!(plain("/home/a/.ssh/config"));
        assert!(!plain("/home/a b/config"));
        assert!(!plain("/home/a'b"));
        assert!(!plain("/home/$USER"));
    }

    /// A lookup of `name` for records of `kind`, in the form a resolver sends it.
    fn query(name: &str, kind: u16) -> Vec<u8> {
        let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in name.split('.') {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.push(0);
        q.extend_from_slice(&kind.to_be_bytes());
        q.extend_from_slice(&[0, 1]);
        q
    }

    /// An address query is answered with an address of the range, and the name is recoverable.
    #[test]
    fn a_lookup_is_answered_with_an_address_that_names_the_host() {
        let mut names = Names::default();
        let reply = answer(&query("Example.COM", 1), &mut names).unwrap();
        assert_eq!(&reply[..2], &[0x12, 0x34]);
        assert_eq!(reply[2] & 0x80, 0x80, "a response");
        assert_eq!(reply[3] & 0x0f, 0, "no error");
        assert_eq!(&reply[6..8], &[0, 1], "one answer");
        let address = Ipv4Addr::new(
            reply[reply.len() - 4],
            reply[reply.len() - 3],
            reply[reply.len() - 2],
            reply[reply.len() - 1],
        );
        assert!(handed_out(address));
        assert_eq!(names.name(address).as_deref(), Some("example.com"));
        // The same name is given the same address.
        let again = answer(&query("example.com", 1), &mut names).unwrap();
        assert_eq!(reply[reply.len() - 4..], again[again.len() - 4..]);
    }

    /// Any other kind of record is answered empty, so that the client uses the address.
    #[test]
    fn a_lookup_for_another_record_is_answered_empty() {
        let mut names = Names::default();
        let reply = answer(&query("example.com", 28), &mut names).unwrap();
        assert_eq!(&reply[6..8], &[0, 0]);
        assert_eq!(reply[3] & 0x0f, 0);
        assert!(names.by_name.is_empty());
    }

    #[test]
    fn a_message_that_is_not_a_query_is_not_answered() {
        let mut names = Names::default();
        let mut response = query("example.com", 1);
        response[2] |= 0x80;
        assert!(answer(&response, &mut names).is_none());
        assert!(answer(&[0; 5], &mut names).is_none());
        let mut truncated = query("example.com", 1);
        truncated.truncate(20);
        assert!(answer(&truncated, &mut names).is_none());
    }

    /// When the range is used up the oldest name gives its address to the newest.
    #[test]
    fn a_used_up_range_gives_the_oldest_address_again() {
        let mut names = Names::default();
        let first = names.address("first.test");
        for i in 1..HANDED_OUT_COUNT {
            names.address(&format!("host{i}.test"));
        }
        let last = names.address("last.test");
        assert_eq!(first, last);
        assert_eq!(names.name(first).as_deref(), Some("last.test"));
        assert!(!names.by_name.contains_key("first.test"));
    }

    #[test]
    fn the_range_the_resolver_hands_out_is_not_a_public_address() {
        assert!(is_blocked(v4("198.18.0.1")));
        assert!(is_blocked(v4("198.19.255.255")));
        assert!(!is_blocked(v4("198.20.0.1")));
        assert!(handed_out(Ipv4Addr::new(198, 18, 0, 0)));
        assert!(handed_out(Ipv4Addr::new(198, 19, 255, 255)));
        assert!(!handed_out(Ipv4Addr::new(198, 20, 0, 0)));
    }

    /// Without a firewall program the namespace still has its proxy, and no resolver.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_namespace_made_without_a_firewall_has_no_resolver() {
        let Ok(made) = netns::create(None) else {
            return;
        };
        assert!(made.dns.is_none());
    }

    #[test]
    fn a_target_is_put_in_normal_form() {
        assert_eq!(
            authority("Example.COM.:443", None),
            Ok(("example.com".into(), 443))
        );
        assert_eq!(authority("[::1]:8080", None), Ok(("::1".into(), 8080)));
        assert_eq!(authority("host", Some(80)), Ok(("host".into(), 80)));
        assert!(authority("host", None).is_err());
        assert!(authority(":80", None).is_err());
    }

    /// A request that is not a request is turned away.
    #[tokio::test]
    async fn a_malformed_request_is_turned_away() {
        let (proxy, _lease) = proxy_with(Fixed::new(yes)).await;
        let answer = exchange(&proxy, "nonsense\r\n\r\n").await;
        assert!(answer.starts_with("HTTP/1.1 400"), "{answer}");
        let https = exchange(&proxy, "GET https://x.test/ HTTP/1.1\r\n\r\n").await;
        assert!(https.starts_with("HTTP/1.1 400"), "{https}");
    }

    /// What a SOCKS5 client reads back after asking `proxy` to connect to `target`.
    async fn socks_connect(
        proxy: &Proxy,
        user: &str,
        target: (u8, &[u8], u16),
    ) -> (TcpStream, [u8; 10]) {
        let (kind, address, port) = target;
        let mut s = TcpStream::connect(proxy.local_addr()).await.unwrap();
        s.write_all(&[5, 1, 2]).await.unwrap();
        let mut chosen = [0u8; 2];
        s.read_exact(&mut chosen).await.unwrap();
        assert_eq!(chosen, [5, 2]);
        let mut auth = vec![1, user.len() as u8];
        auth.extend_from_slice(user.as_bytes());
        auth.extend_from_slice(&[1, b'x']);
        s.write_all(&auth).await.unwrap();
        let mut ok = [0u8; 2];
        s.read_exact(&mut ok).await.unwrap();
        if ok != [1, 0] {
            return (s, [0xff; 10]);
        }
        let mut ask = vec![5, 1, 0, kind];
        if kind == 3 {
            ask.push(address.len() as u8);
        }
        ask.extend_from_slice(address);
        ask.extend_from_slice(&port.to_be_bytes());
        s.write_all(&ask).await.unwrap();
        let mut answer = [0u8; 10];
        s.read_exact(&mut answer).await.unwrap();
        (s, answer)
    }

    /// A SOCKS5 client reaches an agreed host by address or by name, through the same gate.
    #[tokio::test]
    async fn a_socks_client_is_carried_to_an_agreed_host() {
        let (port, _) = echo().await;
        let gate = Fixed::new(yes);
        let (proxy, lease) = proxy_with(gate.clone()).await;
        let by_address = (1, &[127u8, 0, 0, 1][..], port);
        let (mut s, answer) = socks_connect(&proxy, &lease.token, by_address).await;
        assert_eq!(answer[..2], [5, 0]);
        s.write_all(b"ping").await.unwrap();
        let mut back = [0u8; 4];
        s.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"ping");

        // The name is the same host as far as the gate goes.
        let by_name = (3, &b"127.0.0.1"[..], port);
        let (_, answer) = socks_connect(&proxy, &lease.token, by_name).await;
        assert_eq!(answer[..2], [5, 0]);
        assert_eq!(gate.asked(), 1);
    }

    /// A refusal by the gate is "not allowed" in SOCKS5, and the host is never connected to.
    #[tokio::test]
    async fn a_socks_client_is_refused_like_any_other() {
        let (port, connected) = echo().await;
        let (proxy, lease) = proxy_with(Fixed::new(no)).await;
        let (_, answer) = socks_connect(&proxy, &lease.token, (1, &[127, 0, 0, 1], port)).await;
        assert_eq!(answer[..2], [5, 2]);
        assert_eq!(connected.load(Ordering::SeqCst), 0);
    }

    /// A SOCKS5 client without the token of a lease is turned away before it asks for anything.
    #[tokio::test]
    async fn a_socks_client_without_a_lease_is_turned_away() {
        let gate = Fixed::new(yes);
        let (proxy, _lease) = proxy_with(gate.clone()).await;
        let (_, answer) = socks_connect(&proxy, "forged", (1, &[127, 0, 0, 1], 9)).await;
        assert_eq!(answer, [0xff; 10]);
        assert_eq!(gate.asked(), 0);
    }

    /// A gate that agrees to every host and answers the question about volume as told.
    struct Volume {
        agree: bool,
        asked: Mutex<Vec<ApprovalRequest>>,
    }

    impl Volume {
        fn new(agree: bool) -> Arc<Self> {
            Arc::new(Self {
                agree,
                asked: Mutex::new(Vec::new()),
            })
        }

        fn about_volume(&self) -> usize {
            let asked = self.asked.lock().unwrap();
            asked
                .iter()
                .filter(|r| matches!(r.scope, ApprovalScope::Told(_)))
                .count()
        }
    }

    #[async_trait]
    impl Gate for Volume {
        async fn approve(&self, req: ApprovalRequest) -> Approval {
            let volume = matches!(req.scope, ApprovalScope::Told(_));
            self.asked.lock().unwrap().push(req);
            match !volume || self.agree {
                true => Approval::Proceed,
                false => Approval::Denied(Denial::User(None)),
            }
        }
    }

    /// A host that reads what it is sent, counts it, and sends `greeting` first.
    async fn sink(greeting: usize) -> (u16, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let total = Arc::new(AtomicUsize::new(0));
        let counted = total.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let counted = counted.clone();
                tokio::spawn(async move {
                    if greeting > 0 {
                        let _ = sock.write_all(&vec![7u8; greeting]).await;
                    }
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = sock.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        counted.fetch_add(n, Ordering::SeqCst);
                    }
                });
            }
        });
        (port, total)
    }

    /// A proxy whose journal holds `usual` for the loopback host, with a lease decided by `gate`.
    async fn proxy_knowing(usual: Usual, gate: Arc<dyn Gate>) -> (Proxy, Lease, Arc<Journal>) {
        let journal = Arc::new(Journal::default());
        journal
            .usual
            .lock()
            .unwrap()
            .insert("127.0.0.1".into(), usual);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let allow = Arc::new(["127.0.0.1".to_string()].into_iter().collect());
        let proxy = Proxy::serve(listener, allow, journal.clone()).unwrap();
        let lease = proxy.lease(gate, "bash: curl");
        (proxy, lease, journal)
    }

    /// Open a tunnel to the loopback `port` and read the answer line.
    async fn tunnel_to(proxy: &Proxy, lease: &Lease, port: u16) -> TcpStream {
        let mut s = TcpStream::connect(proxy.local_addr()).await.unwrap();
        let req = format!(
            "CONNECT 127.0.0.1:{port} HTTP/1.1\r\n{}\r\n",
            credential(lease)
        );
        s.write_all(req.as_bytes()).await.unwrap();
        let mut head = vec![0u8; 39];
        s.read_exact(&mut head).await.unwrap();
        assert!(String::from_utf8_lossy(&head).starts_with("HTTP/1.1 200"));
        s
    }

    /// Wait until the sink has counted `want` bytes, or give up.
    async fn counted(total: &AtomicUsize, want: usize) -> usize {
        for _ in 0..100 {
            if total.load(Ordering::SeqCst) >= want {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        total.load(Ordering::SeqCst)
    }

    /// An upload larger than any before it, with nothing coming back, is put to the person.
    /// Refused, it never reaches the host.
    #[tokio::test]
    async fn an_unusual_upload_is_asked_about_and_stopped_when_refused() {
        let (port, total) = sink(0).await;
        let gate = Volume::new(false);
        let usual = Usual {
            sent: 100,
            ratio: 2.0,
            ..Default::default()
        };
        let (proxy, lease, journal) = proxy_knowing(usual, gate.clone()).await;
        let mut s = tunnel_to(&proxy, &lease, port).await;
        s.write_all(&[1u8; 1000]).await.unwrap();
        let mut rest = Vec::new();
        let _ = s.read_to_end(&mut rest).await;

        assert_eq!(gate.about_volume(), 1);
        let said = gate.asked.lock().unwrap()[1].summary.clone();
        assert!(said.contains("1.0 KB") && said.contains("100 B"), "{said}");
        assert_eq!(total.load(Ordering::SeqCst), 0);
        let seen = journal.seen();
        assert_eq!(seen[0].1.refused, 1);
    }

    /// Agreed to, the same upload goes through, and it is what is usual from then on.
    #[tokio::test]
    async fn an_unusual_upload_goes_through_when_agreed_and_becomes_usual() {
        let (port, total) = sink(0).await;
        let gate = Volume::new(true);
        let usual = Usual {
            sent: 100,
            ratio: 2.0,
            ..Default::default()
        };
        let (proxy, lease, journal) = proxy_knowing(usual, gate.clone()).await;
        let mut s = tunnel_to(&proxy, &lease, port).await;
        s.write_all(&[1u8; 1000]).await.unwrap();
        s.shutdown().await.unwrap();
        let mut rest = Vec::new();
        let _ = s.read_to_end(&mut rest).await;

        assert_eq!(counted(&total, 1000).await, 1000);
        assert_eq!(gate.about_volume(), 1);
        let learned = journal.usual_for("127.0.0.1").unwrap();
        assert_eq!(learned.sent, 1000);
    }

    /// An upload a little larger than the largest before it is the same traffic, and is not put
    /// to anyone.
    #[tokio::test]
    async fn a_slightly_larger_upload_is_not_asked_about() {
        let usual = Usual {
            sent: 1000,
            ratio: 0.1,
            ..Default::default()
        };
        let (port, total) = sink(0).await;
        let gate = Volume::new(false);
        let (proxy, lease, _) = proxy_knowing(usual, gate.clone()).await;
        let mut s = tunnel_to(&proxy, &lease, port).await;
        s.write_all(&[1u8; 1400]).await.unwrap();
        assert_eq!(counted(&total, 1400).await, 1400);
        assert_eq!(gate.about_volume(), 0);
    }

    /// A proxy whose journal knows what earlier sessions sent to the loopback host.
    async fn proxy_after_sessions(earlier: u64, gate: Arc<dyn Gate>) -> (Proxy, Lease) {
        let journal = Arc::new(Journal {
            earlier: [("127.0.0.1".to_string(), earlier)].into_iter().collect(),
            ..roomy_journal()
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let allow = Arc::new(["127.0.0.1".to_string()].into_iter().collect());
        let proxy = Proxy::serve(listener, allow, journal).unwrap();
        let lease = proxy.lease(gate, "bash: curl");
        (proxy, lease)
    }

    /// An upload cut into connections that are each ordinary still adds up, and is put to the
    /// person once the session has sent several times what an earlier one did.
    #[tokio::test]
    async fn an_upload_split_over_connections_is_asked_about_when_it_adds_up() {
        let (port, total) = sink(0).await;
        let gate = Volume::new(false);
        let (proxy, lease) = proxy_after_sessions(1000, gate.clone()).await;
        for _ in 0..2 {
            let mut s = tunnel_to(&proxy, &lease, port).await;
            s.write_all(&[1u8; 1500]).await.unwrap();
            s.shutdown().await.unwrap();
            let mut rest = Vec::new();
            let _ = s.read_to_end(&mut rest).await;
        }
        assert_eq!(counted(&total, 3000).await, 3000);
        assert_eq!(gate.about_volume(), 0);

        // 3000 sent, and 1500 more takes the session past 4 times the earlier one.
        let mut s = tunnel_to(&proxy, &lease, port).await;
        s.write_all(&[1u8; 1500]).await.unwrap();
        let mut rest = Vec::new();
        let _ = s.read_to_end(&mut rest).await;
        assert_eq!(gate.about_volume(), 1);
        let said = gate.asked.lock().unwrap().last().unwrap().summary.clone();
        assert!(said.contains("in this session"), "{said}");
        assert_eq!(total.load(Ordering::SeqCst), 3000);
    }

    /// Nothing is unusual in a session when no earlier session sent anything.
    #[test]
    fn a_session_has_no_budget_before_an_earlier_one() {
        let journal = Journal::default();
        assert_eq!(journal.session_budget("a.test"), None);
        let journal = Journal {
            earlier: [("a.test".to_string(), 1000)].into_iter().collect(),
            ..Journal::default()
        };
        assert_eq!(journal.session_budget("a.test"), Some((0, 4000)));
        // A host never seen is judged by the largest of the others.
        assert_eq!(journal.session_budget("b.test"), Some((0, 4000)));
    }

    /// What a session sent in all is kept for the next one.
    #[test]
    fn what_a_session_sent_in_all_is_kept_for_the_next() {
        let data = tempfile::tempdir().unwrap();
        crate::paths::with_env(&[("XDG_DATA_HOME", data.path().to_str())], || {
            let here = Path::new("/somewhere/project");
            let first = Journal::kept_for(here);
            first.connected("a.test", 700, 70);
            first.connected("a.test", 500, 50);
            let next = Journal::kept_for(here);
            assert_eq!(next.session_budget("a.test"), Some((0, 4800)));
        });
    }

    /// Two sessions of a project that write at once each keep what the other learned.
    #[test]
    fn two_sessions_keep_each_others_habits() {
        let data = tempfile::tempdir().unwrap();
        crate::paths::with_env(&[("XDG_DATA_HOME", data.path().to_str())], || {
            let here = Path::new("/somewhere/project");
            let writers: Vec<_> = (0..8u64)
                .map(|n| {
                    std::thread::spawn(move || {
                        let held = BTreeMap::from([(
                            format!("h{n}.test"),
                            Usual {
                                sent: 100 * (n + 1),
                                ratio: 1.0,
                                session: 0,
                            },
                        )]);
                        write_usual(here, &held, 1, &Mutex::new(0));
                    })
                })
                .collect();
            for w in writers {
                w.join().unwrap();
            }
            let all = read_usual_all().unwrap();
            let kept = &all[&here.display().to_string()];
            assert_eq!(kept.len(), 8, "{kept:?}");
            assert_eq!(kept["h7.test"].sent, 800);
        });
    }

    /// A small upload, or one that follows a large download, is not put to anyone.
    #[tokio::test]
    async fn an_ordinary_upload_is_not_asked_about() {
        let usual = Usual {
            sent: 100,
            ratio: 2.0,
            ..Default::default()
        };
        let (port, total) = sink(0).await;
        let gate = Volume::new(false);
        let (proxy, lease, _) = proxy_knowing(usual, gate.clone()).await;
        let mut s = tunnel_to(&proxy, &lease, port).await;
        s.write_all(&[1u8; 50]).await.unwrap();
        assert_eq!(counted(&total, 50).await, 50);
        assert_eq!(gate.about_volume(), 0);

        // The same size as the unusual one, after 100 KB came the other way.
        let (port, total) = sink(100_000).await;
        let gate = Volume::new(false);
        let (proxy, lease, _) = proxy_knowing(usual, gate.clone()).await;
        let mut s = tunnel_to(&proxy, &lease, port).await;
        let mut downloaded = vec![0u8; 100_000];
        s.read_exact(&mut downloaded).await.unwrap();
        s.write_all(&[1u8; 1000]).await.unwrap();
        assert_eq!(counted(&total, 1000).await, 1000);
        assert_eq!(gate.about_volume(), 0);
    }

    /// Before anything is known of the project, the first upload to a host is put to the person.
    #[tokio::test]
    async fn the_first_upload_of_a_project_is_asked_about() {
        let (port, total) = sink(0).await;
        let gate = Volume::new(true);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let allow = Arc::new(["127.0.0.1".to_string()].into_iter().collect());
        let proxy = Proxy::serve(listener, allow, Arc::new(Journal::default())).unwrap();
        let lease = proxy.lease(gate.clone(), "bash: curl");
        let mut s = tunnel_to(&proxy, &lease, port).await;
        s.write_all(&[1u8; 5000]).await.unwrap();
        assert_eq!(counted(&total, 5000).await, 5000);
        assert_eq!(gate.about_volume(), 1);
        let said = gate.asked.lock().unwrap().last().unwrap().summary.clone();
        assert!(said.contains("nothing is known yet"), "{said}");
    }

    /// A host never seen is judged by the largest of the hosts that were.
    #[test]
    fn an_unseen_host_is_judged_by_the_largest_seen() {
        let journal = Journal::default();
        assert_eq!(journal.usual_for("a.test"), None);
        journal.connected("a.test", 500, 10_000);
        journal.connected("b.test", 9_000, 100);
        journal.reached("a.test", 443);
        journal.reached("a.test", 22);
        let ports: Vec<u16> = journal.seen()[0].1.ports.iter().copied().collect();
        assert_eq!(ports, [22, 443]);
        let a = journal.usual_for("a.test").unwrap();
        assert_eq!(a.sent, 500);
        let unseen = journal.usual_for("c.test").unwrap();
        assert_eq!(unseen.sent, 9_000);
        assert_eq!(unseen.ratio, 90.0);
    }

    /// What is usual is kept for its project, and for that project only.
    #[test]
    fn what_is_usual_is_kept_for_its_project() {
        let data = tempfile::tempdir().unwrap();
        crate::paths::with_env(&[("XDG_DATA_HOME", data.path().to_str())], || {
            let here = Path::new("/somewhere/project");
            Journal::kept_for(here).connected("a.test", 700, 70);
            let next = Journal::kept_for(here);
            assert_eq!(next.usual_for("a.test").unwrap().sent, 700);
            assert_eq!(
                Journal::kept_for(Path::new("/elsewhere")).usual_for("a.test"),
                None
            );
        });
    }

    #[test]
    fn a_size_is_written_for_a_person() {
        assert_eq!(size(0), "0 B");
        assert_eq!(size(999), "999 B");
        assert_eq!(size(1_000), "1.0 KB");
        assert_eq!(size(12_300_000), "12.3 MB");
        assert_eq!(size(5_000_000_000), "5.0 GB");
    }
}
