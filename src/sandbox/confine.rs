//! Making the root a boundary for the commands the agent runs.
//!
//! The path sandbox governs the tools that take paths.
//! A shell takes a string, and an approved command runs with the user's privileges.
//! A confined command and everything it starts may write only beneath the directories its
//! [`Sandbox`] names, and to the devices under `/dev`.
//! It may read everything but what the sandbox hides.
//! It may reach the network only as the sandbox lets it: not at all, through the proxy of
//! [`egress`](super::egress), or freely.
//!
//! | System | Mechanism |
//! |---|---|
//! | Linux | Landlock: the command gives up the access before it runs, for good |
//! | macOS | Seatbelt: the command runs under `sandbox-exec` with a profile written here |

use std::path::PathBuf;
use std::sync::Arc;

use super::egress::{Egress, Gate, Lease};

/// What has to be kept until a confined command has ended.
///
/// The ruleset is needed until the command has been spawned.
/// The lease is the command's right to the proxy, and it ends with the command.
#[derive(Default)]
pub struct Held {
    _ruleset: Option<std::os::fd::OwnedFd>,
    _lease: Option<Lease>,
}

impl Held {
    /// The connections of the command that were turned away, and why.
    pub fn turned_away(&self) -> Vec<String> {
        self._lease
            .as_ref()
            .map(Lease::turned_away)
            .unwrap_or_default()
    }
}

/// Who asks for the connections of a command, and how it is named in the question.
pub struct Asking {
    pub gate: Arc<dyn Gate>,
    pub origin: String,
}

/// What confines commands on this system, for saying so.
pub const MECHANISM: &str = if cfg!(target_os = "macos") {
    "Seatbelt"
} else {
    "Landlock"
};

/// Whether commands can be confined here.
pub fn available() -> bool {
    match cfg!(target_os = "macos") {
        true => std::path::Path::new(seatbelt::SANDBOX_EXEC).exists(),
        false => abi().is_some(),
    }
}

/// Whether a sandbox here can keep a command off the network.
pub fn cuts_network() -> bool {
    match cfg!(target_os = "macos") {
        true => available(),
        false => abi().is_some_and(|v| v >= 4),
    }
}

/// A command that runs `program` with `args`, in `sandbox` when there is one.
///
/// It also returns what must be kept until it has been spawned.
/// A sandbox that cannot be applied is an error, never a command left unconfined.
pub fn command(
    sandbox: Option<&Sandbox>,
    asking: Option<Asking>,
    program: &str,
    args: &[&str],
) -> std::io::Result<(tokio::process::Command, Held)> {
    match sandbox {
        Some(s) => s.command(asking, program, args),
        None => {
            let mut cmd = tokio::process::Command::new(program);
            cmd.args(args);
            Ok((cmd, Held::default()))
        }
    }
}

/// How far the commands the agent runs are confined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Level {
    /// Writes nowhere but the devices.
    ReadOnly,
    /// Writes in the project, the temporary directories and the build caches.
    Workspace,
    /// Unconfined.
    Off,
}

impl Level {
    pub fn label(self) -> &'static str {
        match self {
            Level::ReadOnly => "read-only",
            Level::Workspace => "workspace",
            Level::Off => "off",
        }
    }
}

/// How a confined command reaches the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Network {
    /// It reaches nothing.
    Closed,
    /// It reaches what the proxy lets through, host by host.
    Proxy,
    /// It reaches everything.
    Open,
    /// It goes through the proxy, which lets every host through and records it.
    /// A rule that refuses a host still does.
    Audit,
}

impl Network {
    pub fn label(self) -> &'static str {
        match self {
            Network::Closed => "closed",
            Network::Proxy => "proxy",
            Network::Open => "open",
            Network::Audit => "audit",
        }
    }
}

/// What a confined command may do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sandbox {
    pub level: Level,
    /// The directories and files it may write beneath, besides the devices.
    pub writable: Vec<PathBuf>,
    /// The directories and files it may not read.
    pub hidden: Vec<PathBuf>,
    /// How it reaches the network.
    pub network: Network,
    /// The proxy and the network namespace of the session, when it has them.
    pub egress: Option<Arc<Egress>>,
    /// The environment variables taken out of what it inherits.
    pub unset: Vec<String>,
}

impl Sandbox {
    /// A command that runs `program` with `args` in this sandbox, and what must be kept
    /// until it has ended.
    ///
    /// Through the proxy, the command is given a lease and the environment that uses it.
    /// `asking` says who answers the proxy's questions.
    /// A command that is to go through the proxy without anyone to ask is refused.
    pub fn command(
        &self,
        asking: Option<Asking>,
        program: &str,
        args: &[&str],
    ) -> std::io::Result<(tokio::process::Command, Held)> {
        let lease = match self.network {
            Network::Proxy | Network::Audit => {
                let egress = self
                    .egress
                    .as_ref()
                    .ok_or_else(|| std::io::Error::other("the network proxy is not running"))?;
                let asking = asking
                    .ok_or_else(|| std::io::Error::other("nobody answers for this command"))?;
                // An audit has the questions answered by recording the host.
                let gate: std::sync::Arc<dyn super::egress::Gate> = match self.network {
                    Network::Audit => std::sync::Arc::new(super::egress::Recording(asking.gate)),
                    _ => asking.gate,
                };
                Some(egress.lease(gate, asking.origin))
            }
            Network::Closed | Network::Open => None,
        };
        let (mut cmd, ruleset) = if cfg!(target_os = "macos") {
            let cmd = seatbelt::command(&seatbelt::profile(self), program, args);
            (cmd, None)
        } else {
            let mut cmd = tokio::process::Command::new(program);
            cmd.args(args);
            let fd = self.confine(&mut cmd)?;
            (cmd, Some(fd))
        };
        for var in &self.unset {
            cmd.env_remove(var);
        }
        if let Some(lease) = &lease {
            cmd.envs(lease.env());
        }
        Ok((
            cmd,
            Held {
                _ruleset: ruleset,
                _lease: lease,
            },
        ))
    }

    /// The namespaces the command joins, when its network is cut or filtered by them.
    fn namespaces(&self) -> Option<[std::os::fd::RawFd; 3]> {
        match self.network {
            Network::Open => None,
            Network::Closed | Network::Proxy | Network::Audit => {
                self.egress.as_ref().and_then(|e| e.namespaces())
            }
        }
    }

    /// Whether Landlock leaves TCP alone.
    ///
    /// A namespace has already cut the network, and an open network has not been cut.
    /// A closed network without a namespace is cut by Landlock.
    fn tcp_free(&self) -> bool {
        self.network != Network::Closed || self.namespaces().is_some()
    }

    /// Confine `cmd` with Landlock, and put it in the network namespace when there is one.
    ///
    /// It takes out the variables the command must not see.
    /// It has the command give up what the sandbox does not allow before it runs.
    /// The descriptor returned must outlive the spawn.
    fn confine(&self, cmd: &mut tokio::process::Command) -> std::io::Result<std::os::fd::OwnedFd> {
        for var in &self.unset {
            cmd.env_remove(var);
        }
        let fd = ruleset(&self.writable, &self.hidden, self.tcp_free())?;
        use std::os::fd::AsRawFd;
        let raw = fd.as_raw_fd();
        let spaces = self.namespaces().unwrap_or([-1; 3]);
        // SAFETY: `restrict_after_fork` makes a few syscalls and nothing else.
        // It does no allocation and takes no locks, which is what a post-fork hook may do.
        // `raw` stays open because the caller keeps `fd` until the spawn is over.
        // `spaces` are owned by the session's egress, which the sandbox keeps alive.
        unsafe {
            cmd.pre_exec(move || restrict_after_fork(raw, spaces));
        }
        Ok(fd)
    }

    /// What a command that failed in it is told about it.
    pub fn note(&self) -> String {
        let writes = match self.level {
            Level::ReadOnly => "may not write any file",
            _ => "may write only in the project, the temporary directory and the build caches",
        };
        let network = match self.network {
            Network::Open => "",
            Network::Proxy => ", and reaches the network only through the proxy, host by host",
            Network::Audit => {
                ", and reaches the network through the proxy, which records each host"
            }
            Network::Closed => ", and may not open a network connection",
        };
        format!(
            "It ran in a sandbox: it {writes}{network}. If the sandbox is what stopped it, the \
             same call with outside_sandbox set asks the user to run it unconfined."
        )
    }
}

/// The macOS sandbox: a profile in Seatbelt's language, and the command that runs under it.
///
/// Built on every system, so what it writes is tested wherever the tests run.
/// The profile allows everything the Linux sandbox does not govern, then takes away what it
/// does.
/// That is writes outside the writable paths and the devices, reads of what is hidden, and TCP.
mod seatbelt {
    use std::path::{Path, PathBuf};

    pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

    /// The path as the system names it.
    /// `/tmp` is `/private/tmp` there, and a profile matches the real path.
    fn real(path: &Path) -> PathBuf {
        path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
    }

    /// A string of the profile's language.
    fn quoted(path: &Path) -> String {
        let text = path.to_string_lossy();
        format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
    }

    /// What `path` is to a rule: everything beneath a directory, or one file.
    fn filter(path: &Path) -> String {
        let real = real(path);
        match real.is_file() {
            true => format!("(literal {})", quoted(&real)),
            false => format!("(subpath {})", quoted(&real)),
        }
    }

    /// The profile for `sandbox`.
    /// The last rule that matches decides.
    pub fn profile(sandbox: &super::Sandbox) -> String {
        profile_for(sandbox, sandbox.egress.as_ref().map(|e| e.port()))
    }

    /// The profile for `sandbox`, with the proxy listening on `proxy_port` when there is one.
    pub fn profile_for(sandbox: &super::Sandbox, proxy_port: Option<u16>) -> String {
        let mut out = vec![
            "(version 1)".to_string(),
            "(allow default)".to_string(),
            "(deny file-write*)".to_string(),
        ];
        for path in &sandbox.writable {
            out.push(format!("(allow file-write* {})", filter(path)));
        }
        out.push("(allow file-write-data file-ioctl (subpath \"/dev\"))".to_string());
        for path in &sandbox.hidden {
            out.push(format!("(deny file-read* {})", filter(path)));
        }
        if sandbox.network != super::Network::Open {
            out.push("(deny network-outbound (remote ip))".to_string());
            out.push("(deny network-bind (local ip))".to_string());
            out.push("(deny network-inbound (local ip))".to_string());
            // A socket that is a file is reachable only under the paths the command may write to.
            // With the network open, the system's own services keep working, such as the resolver.
            out.push("(deny network-outbound (remote unix-socket))".to_string());
            for path in &sandbox.writable {
                out.push(format!(
                    "(allow network-outbound (remote unix-socket {}))",
                    filter(path)
                ));
            }
        }
        // Through the proxy, the one address it listens on stays reachable.
        if let (super::Network::Proxy | super::Network::Audit, Some(port)) =
            (sandbox.network, proxy_port)
        {
            out.push(format!(
                "(allow network-outbound (remote ip \"localhost:{port}\"))"
            ));
        }
        out.join("\n")
    }

    /// `program` with `args`, run under `profile`.
    pub fn command(profile: &str, program: &str, args: &[&str]) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(SANDBOX_EXEC);
        cmd.arg("-p").arg(profile).arg("--").arg(program).args(args);
        cmd
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    const SYS_CREATE_RULESET: libc::c_long = 444;
    const SYS_ADD_RULE: libc::c_long = 445;
    const SYS_RESTRICT_SELF: libc::c_long = 446;

    const CREATE_RULESET_VERSION: u32 = 1;
    const RULE_PATH_BENEATH: libc::c_int = 1;

    // The accesses, by the ABI that introduced each.
    const WRITE_FILE: u64 = 1 << 1;
    const READ_FILE: u64 = 1 << 2;
    const READ_DIR: u64 = 1 << 3;
    const REMOVE_DIR: u64 = 1 << 4;
    const REMOVE_FILE: u64 = 1 << 5;
    const MAKE_CHAR: u64 = 1 << 6;
    const MAKE_DIR: u64 = 1 << 7;
    const MAKE_REG: u64 = 1 << 8;
    const MAKE_SOCK: u64 = 1 << 9;
    const MAKE_FIFO: u64 = 1 << 10;
    const MAKE_BLOCK: u64 = 1 << 11;
    const MAKE_SYM: u64 = 1 << 12;
    /// Moving a file between directories. ABI 2.
    const REFER: u64 = 1 << 13;
    /// ABI 3. Without it, a command can empty a file it may not write to.
    const TRUNCATE: u64 = 1 << 14;
    /// Connecting to a socket that is a file. ABI 9.
    /// Without it, a command can reach any service that listens on a path.
    const RESOLVE_UNIX: u64 = 1 << 16;

    #[repr(C)]
    struct RulesetAttr {
        handled_access_fs: u64,
        /// ABI 4. Left out of the size given to the kernel when no network is handled.
        handled_access_net: u64,
    }

    const BIND_TCP: u64 = 1 << 0;
    const CONNECT_TCP: u64 = 1 << 1;

    #[repr(C, packed)]
    struct PathBeneathAttr {
        allowed_access: u64,
        parent_fd: i32,
    }

    /// The kernel's Landlock ABI, or `None` where there is no Landlock.
    pub fn abi() -> Option<u32> {
        // SAFETY: the documented way to ask: a null attribute with the version flag.
        let v = unsafe {
            libc::syscall(
                SYS_CREATE_RULESET,
                std::ptr::null::<RulesetAttr>(),
                0usize,
                CREATE_RULESET_VERSION,
            )
        };
        (v > 0).then_some(v as u32)
    }

    /// Everything write-shaped this kernel knows how to govern, and the sockets that are files.
    fn writes_for(abi: u32) -> u64 {
        let mut bits = WRITE_FILE
            | REMOVE_DIR
            | REMOVE_FILE
            | MAKE_CHAR
            | MAKE_DIR
            | MAKE_REG
            | MAKE_SOCK
            | MAKE_FIFO
            | MAKE_BLOCK
            | MAKE_SYM;
        if abi >= 2 {
            bits |= REFER;
        }
        if abi >= 3 {
            bits |= TRUNCATE;
        }
        if abi >= 9 {
            bits |= RESOLVE_UNIX;
        }
        bits
    }

    /// Build the ruleset in the parent.
    ///
    /// Creating the ruleset and adding a rule per allowed directory both allocate and open files.
    /// Doing that between `fork` and `exec` in a process with other threads can deadlock in the
    /// allocator.
    /// What is left for the child is 2 bare syscalls.
    pub fn ruleset(writable: &[PathBuf], hidden: &[PathBuf], network: bool) -> io::Result<OwnedFd> {
        let Some(abi) = abi() else {
            return Err(io::Error::other("this kernel has no Landlock"));
        };
        let reads = match hidden.is_empty() {
            true => 0,
            false => READ_FILE | READ_DIR,
        };
        // With no rule for a port, every TCP bind and connect is refused.
        let cut = !network && abi >= 4;
        let attr = RulesetAttr {
            handled_access_fs: writes_for(abi) | reads,
            handled_access_net: if cut { BIND_TCP | CONNECT_TCP } else { 0 },
        };
        let size = match cut {
            true => std::mem::size_of::<RulesetAttr>(),
            false => std::mem::size_of::<u64>(),
        };
        // SAFETY: `attr` is live for the call and holds at least the size the kernel is told.
        let fd = unsafe {
            libc::syscall(
                SYS_CREATE_RULESET,
                &attr as *const RulesetAttr,
                size,
                0usize,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh descriptor the kernel just handed us, owned from here.
        let ruleset = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };

        for path in writable {
            // A rule on a file takes only the accesses a file has.
            let access = match path.is_dir() {
                true => writes_for(abi),
                false => writes_for(abi) & (WRITE_FILE | TRUNCATE),
            };
            allow_beneath(&ruleset, path, access)?;
        }
        // Writing to a device is how a command discards output or talks to a terminal.
        // A device cannot be made or removed under this rule.
        let devices = match abi {
            1 | 2 => WRITE_FILE,
            _ => WRITE_FILE | TRUNCATE,
        };
        allow_beneath(&ruleset, Path::new("/dev"), devices)?;
        if !hidden.is_empty() {
            let hidden: Vec<PathBuf> = hidden
                .iter()
                .map(|h| h.canonicalize().unwrap_or_else(|_| h.clone()))
                .collect();
            for (path, whole) in readable(Path::new("/"), &hidden) {
                let access = match (whole, path.is_dir()) {
                    (true, true) => READ_FILE | READ_DIR,
                    (true, false) => READ_FILE,
                    (false, _) => READ_DIR,
                };
                allow_beneath(&ruleset, &path, access)?;
            }
        }
        Ok(ruleset)
    }

    /// What may be read beneath `dir` with `hidden` kept out.
    /// Each path comes with whether all of it may be read or only the names in it.
    /// A directory with something hidden below it has its names readable and is walked into.
    /// Names stay readable all the way down, contents of hidden paths do not.
    pub(super) fn readable(dir: &Path, hidden: &[PathBuf]) -> Vec<(PathBuf, bool)> {
        let mut out = Vec::new();
        walk(dir, hidden, &mut out);
        out
    }

    fn walk(dir: &Path, hidden: &[PathBuf], out: &mut Vec<(PathBuf, bool)>) {
        if hidden.iter().any(|h| h == dir) {
            return;
        }
        if !hidden.iter().any(|h| h.starts_with(dir)) {
            out.push((dir.to_path_buf(), true));
            return;
        }
        out.push((dir.to_path_buf(), false));
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // A link is followed by the rule, so one that leads above a hidden path would
            // open it again. Its name is enough to list it.
            let is_link = entry.file_type().map(|t| t.is_symlink()).unwrap_or(false);
            if is_link && hidden.iter().any(|h| h.starts_with(dir)) {
                match path.canonicalize() {
                    Ok(target) if hidden.iter().any(|h| h.starts_with(&target)) => continue,
                    Ok(target) if hidden.iter().any(|h| target.starts_with(h)) => continue,
                    _ => {}
                }
            }
            walk(&path, hidden, out);
        }
    }

    fn allow_beneath(ruleset: &OwnedFd, dir: &Path, access: u64) -> io::Result<()> {
        let c_path = std::ffi::CString::new(dir.as_os_str().as_bytes())
            .map_err(|_| io::Error::other("path contains a null byte"))?;
        // SAFETY: a valid C string, and O_PATH opens the directory as a reference only.
        let parent = unsafe { libc::open(c_path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if parent < 0 {
            // A path that does not open cannot be reached through this rule either.
            // This is not fatal: the point is what the command may reach.
            tracing::debug!(path = %dir.display(), "not adding a landlock rule for a path that does not open");
            return Ok(());
        }
        // SAFETY: owned so the descriptor is closed however this returns.
        let parent = unsafe { OwnedFd::from_raw_fd(parent) };

        let rule = PathBeneathAttr {
            allowed_access: access,
            parent_fd: parent.as_raw_fd(),
        };
        // SAFETY: both descriptors are open and `rule` is live for the call.
        let rc = unsafe {
            libc::syscall(
                SYS_ADD_RULE,
                ruleset.as_raw_fd(),
                RULE_PATH_BENEATH,
                &rule as *const PathBeneathAttr,
                0usize,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Join the namespaces when there are some, then apply the ruleset to this process.
    /// Runs in the child, between `fork` and `exec`.
    ///
    /// A few syscalls and nothing else: no allocation, no locks, nothing that another thread
    /// in the parent could have been holding when the fork happened.
    /// The user namespace comes first, because joining the network namespace takes the rights
    /// that only it gives.
    ///
    /// # Safety
    /// `fd` must be an open Landlock ruleset descriptor.
    /// `spaces` must be open descriptors of the user, network and mount namespaces, in that
    /// order, or negative for none.
    pub unsafe fn restrict_after_fork(fd: RawFd, spaces: [RawFd; 3]) -> io::Result<()> {
        let [user, net, mount] = spaces;
        if user >= 0 {
            // Joining the mount namespace puts the working directory at its root.
            let mut here = [0u8; 4096];
            if libc::getcwd(here.as_mut_ptr().cast(), here.len()).is_null() {
                return Err(io::Error::last_os_error());
            }
            if libc::setns(user, libc::CLONE_NEWUSER) != 0
                || libc::setns(net, libc::CLONE_NEWNET) != 0
                || libc::setns(mount, libc::CLONE_NEWNS) != 0
                || libc::chdir(here.as_ptr().cast()) != 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        // Landlock refuses to apply without this.
        // It stops a confined command from regaining privileges through a setuid binary.
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::syscall(SYS_RESTRICT_SELF, fd, 0usize) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
pub use linux::{abi, restrict_after_fork, ruleset};

#[cfg(not(target_os = "linux"))]
mod elsewhere {
    use std::io;
    use std::os::fd::{OwnedFd, RawFd};
    use std::path::PathBuf;

    pub fn abi() -> Option<u32> {
        None
    }

    pub fn ruleset(
        _writable: &[PathBuf],
        _hidden: &[PathBuf],
        _network: bool,
    ) -> io::Result<OwnedFd> {
        Err(io::Error::other("shell confinement needs Linux or macOS"))
    }

    /// # Safety
    /// Never called: there is no ruleset to apply.
    pub unsafe fn restrict_after_fork(_fd: RawFd, _spaces: [RawFd; 3]) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
pub use elsewhere::{abi, restrict_after_fork, ruleset};

#[cfg(test)]
mod tests {
    use super::*;

    /// The profile allows the writable paths and the devices, takes away what is hidden and,
    /// without the network, TCP. A later rule wins, so what is hidden comes after.
    #[test]
    fn the_macos_profile_says_what_the_sandbox_allows() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().canonicalize().unwrap();
        std::fs::write(dir.path().join("secret"), "x").unwrap();
        let sandbox = Sandbox {
            level: Level::Workspace,
            writable: vec![dir.path().to_path_buf(), PathBuf::from("/no/such/dir")],
            hidden: vec![
                dir.path().join("secret"),
                PathBuf::from("/a \"quoted\" dir"),
            ],
            network: Network::Closed,
            egress: None,
            unset: Vec::new(),
        };
        let profile = seatbelt::profile(&sandbox);
        let lines: Vec<&str> = profile.lines().collect();
        assert_eq!(
            lines[..3],
            ["(version 1)", "(allow default)", "(deny file-write*)"]
        );
        let at = |needle: &str| {
            lines
                .iter()
                .position(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("{needle} is not in\n{profile}"))
        };
        assert!(
            at(&format!(
                "(allow file-write* (subpath \"{}\"))",
                real.display()
            )) < at(&format!(
                "(deny file-read* (literal \"{}/secret\"))",
                real.display()
            ))
        );
        at("(allow file-write* (subpath \"/no/such/dir\"))");
        at(r#"(deny file-read* (subpath "/a \"quoted\" dir"))"#);
        at("(deny network-outbound (remote ip))");
        // A socket that is a file is cut, then let back in under the writable paths.
        assert!(
            at("(deny network-outbound (remote unix-socket))")
                < at(&format!(
                    "(allow network-outbound (remote unix-socket (subpath \"{}\")))",
                    real.display()
                )),
            "{profile}"
        );
        let open = seatbelt::profile(&Sandbox {
            network: Network::Open,
            ..sandbox.clone()
        });
        assert!(!open.contains("network"), "{open}");
        // Through the proxy, TCP is cut and the proxy's own address is let back in after it.
        let proxied = Sandbox {
            network: Network::Proxy,
            ..sandbox
        };
        let through = seatbelt::profile_for(&proxied, Some(3128));
        let lines: Vec<&str> = through.lines().collect();
        let cut = lines
            .iter()
            .position(|l| *l == "(deny network-outbound (remote ip))")
            .unwrap();
        let back = lines
            .iter()
            .position(|l| *l == "(allow network-outbound (remote ip \"localhost:3128\"))")
            .unwrap();
        assert!(cut < back, "{through}");
        assert!(!seatbelt::profile_for(&proxied, None).contains("localhost"));
        let cmd = seatbelt::command(&profile, "/bin/sh", &["-lc", "ls"]);
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(cmd.as_std().get_program(), seatbelt::SANDBOX_EXEC);
        assert_eq!(args, ["-p", profile.as_str(), "--", "/bin/sh", "-lc", "ls"]);
    }

    /// Whether it is available is a fact about the machine.
    /// The answer is a fact, not a guess: the shell is confined or it is not.
    #[test]
    fn availability_is_reported_honestly() {
        match abi() {
            Some(v) => assert!(v >= 1, "an ABI of {v} is not a version"),
            None => {
                // Then building a ruleset must fail rather than silently confine nothing.
                assert!(ruleset(&[std::env::temp_dir()], &[], true).is_err());
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_ruleset_is_built_for_the_directories_given() {
        if abi().is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        assert!(ruleset(&[dir.path().to_path_buf()], &[], true).is_ok());
    }

    /// A directory that is not there is not a reason to refuse to confine anything.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_missing_writable_path_is_skipped_rather_than_fatal() {
        if abi().is_none() {
            return;
        }
        assert!(ruleset(
            &[std::path::PathBuf::from("/no/such/directory/anywhere")],
            &[],
            true
        )
        .is_ok());
    }

    /// A hidden path is left out.
    /// The directories above it keep their names readable.
    /// Everything beside them is readable whole.
    #[cfg(target_os = "linux")]
    #[test]
    fn what_is_hidden_is_left_out_of_what_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::create_dir_all(home.join("work")).unwrap();
        std::fs::write(home.join(".netrc"), "machine x").unwrap();
        std::os::unix::fs::symlink(&home, dir.path().join("link")).unwrap();
        let hidden = [home.join(".ssh"), home.join(".netrc")];
        let mut seen = linux::readable(dir.path(), &hidden);
        seen.sort();
        assert_eq!(
            seen,
            [
                (dir.path().to_path_buf(), false),
                (home.clone(), false),
                (home.join("work"), true),
            ]
        );
    }

    /// A confined command reads what is not hidden, and not what is.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_hidden_file_cannot_be_read() {
        if abi().is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("secret"), "hunter2").unwrap();
        std::fs::write(dir.path().join("open"), "fine").unwrap();
        let sandbox = Sandbox {
            level: Level::Workspace,
            writable: vec![dir.path().to_path_buf()],
            hidden: vec![dir.path().join("secret")],
            network: Network::Open,
            egress: None,
            unset: Vec::new(),
        };
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-c")
            .arg("cat open; cat secret")
            .current_dir(dir.path());
        let fd = sandbox.confine(&mut cmd).unwrap();
        let out = cmd.output().await.unwrap();
        drop(fd);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("fine"), "{stdout}");
        assert!(!stdout.contains("hunter2"), "{stdout}");
    }

    /// A command in a sandbox without the network cannot connect, even to this machine.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_command_without_the_network_cannot_connect() {
        if abi().is_none_or(|v| v < 4) {
            return;
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let run = |network: Network| {
            let sandbox = Sandbox {
                level: Level::Workspace,
                writable: Vec::new(),
                hidden: Vec::new(),
                network,
                egress: None,
                unset: Vec::new(),
            };
            async move {
                let mut cmd = tokio::process::Command::new("/bin/bash");
                cmd.arg("-c").arg(format!(
                    "exec 3<>/dev/tcp/127.0.0.1/{port} && echo connected"
                ));
                let fd = sandbox.confine(&mut cmd).unwrap();
                let out = cmd.output().await.unwrap();
                drop(fd);
                String::from_utf8_lossy(&out.stdout).contains("connected")
            }
        };
        if !std::path::Path::new("/bin/bash").exists() {
            return;
        }
        assert!(run(Network::Open).await);
        assert!(!run(Network::Closed).await);
    }

    /// A command reaches a socket that is a file only where it may write.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_command_cannot_connect_to_a_socket_outside_its_directories() {
        if abi().is_none_or(|v| v < 9) || !std::path::Path::new("/usr/bin/python3").exists() {
            return;
        }
        let service = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let socket = service.path().join("agent.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let run = |writable: PathBuf| {
            let socket = socket.clone();
            async move {
                let sandbox = Sandbox {
                    level: Level::Workspace,
                    writable: vec![writable],
                    hidden: Vec::new(),
                    network: Network::Open,
                    egress: None,
                    unset: Vec::new(),
                };
                let mut cmd = tokio::process::Command::new("/usr/bin/python3");
                cmd.arg("-c").arg(format!(
                    "import socket; s = socket.socket(socket.AF_UNIX); \
                     s.connect({:?}); print('connected')",
                    socket.display().to_string()
                ));
                let fd = sandbox.confine(&mut cmd).unwrap();
                let out = cmd.output().await.unwrap();
                drop(fd);
                String::from_utf8_lossy(&out.stdout).contains("connected")
            }
        };
        assert!(run(service.path().to_path_buf()).await);
        assert!(!run(elsewhere.path().to_path_buf()).await);
    }

    /// A gate that answers the same way to everything.
    #[cfg(target_os = "linux")]
    struct Answer(bool);

    #[cfg(target_os = "linux")]
    #[async_trait::async_trait]
    impl Gate for Answer {
        async fn approve(
            &self,
            _req: crate::sandbox::approval::ApprovalRequest,
        ) -> crate::sandbox::approval::Approval {
            match self.0 {
                true => crate::sandbox::approval::Approval::Proceed,
                false => crate::sandbox::approval::Approval::Denied(
                    crate::sandbox::approval::Denial::User(None),
                ),
            }
        }
    }

    /// What a command prints, run in a sandbox with the network `network`.
    ///
    /// `None` where this machine cannot make the namespace, which the tests then skip.
    #[cfg(target_os = "linux")]
    async fn printed(network: Network, agree: bool, script: &str) -> Option<String> {
        if !available() || !std::path::Path::new("/bin/bash").exists() {
            return None;
        }
        let allow = Arc::new(["127.0.0.1".to_string()].into_iter().collect());
        let egress = Egress::start(allow, Default::default()).await.ok()?;
        let sandbox = Sandbox {
            level: Level::Workspace,
            writable: Vec::new(),
            hidden: Vec::new(),
            network,
            egress: Some(Arc::new(egress)),
            unset: Vec::new(),
        };
        let asking = Asking {
            gate: Arc::new(Answer(agree)),
            origin: "bash: test".into(),
        };
        let (mut cmd, held) = sandbox
            .command(Some(asking), "/bin/bash", &["-c", script])
            .unwrap();
        let out = cmd.output().await.unwrap();
        drop(held);
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// A service on this machine's loopback, which answers once and hangs up.
    #[cfg(target_os = "linux")]
    fn service() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for _ in 0..8 {
                let _ = listener.accept();
            }
        });
        port
    }

    /// The command reaches the service directly: what the test is measured against.
    #[cfg(target_os = "linux")]
    fn direct(port: u16) -> String {
        format!("exec 3<>/dev/tcp/127.0.0.1/{port} && echo connected")
    }

    /// The command asks the proxy for a tunnel to the service and prints the answer line.
    #[cfg(target_os = "linux")]
    fn tunnel(port: u16) -> String {
        format!(
            "url=$https_proxy; hostport=${{url#*@}}; proxy=${{hostport##*:}}; \
             user=${{url#http://}}; user=${{user%%:*}}; \
             auth=$(printf '%s:x' \"$user\" | base64); \
             exec 3<>/dev/tcp/127.0.0.1/$proxy; \
             printf 'CONNECT 127.0.0.1:{port} HTTP/1.1\\r\\nProxy-Authorization: Basic %s\\r\\n\\r\\n' \"$auth\" >&3; \
             head -1 <&3"
        )
    }

    /// A closed network is the namespace's own loopback: the service on this machine's is out
    /// of reach, and an open network reaches it.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_closed_network_does_not_reach_this_machines_loopback() {
        let port = service();
        let Some(open) = printed(Network::Open, true, &direct(port)).await else {
            return;
        };
        assert!(open.contains("connected"), "{open}");
        let closed = printed(Network::Closed, true, &direct(port)).await.unwrap();
        assert!(!closed.contains("connected"), "{closed}");
    }

    /// The namespace keeps the identity of the one who runs the command.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_command_in_the_namespace_keeps_its_identity() {
        let Some(seen) = printed(Network::Closed, true, "id -u").await else {
            return;
        };
        // SAFETY: a plain call that returns an identity.
        let own = unsafe { libc::getuid() };
        assert_eq!(seen.trim(), own.to_string());
    }

    /// Through the proxy a command reaches the host the gate agrees to, and only that way.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_proxied_command_reaches_what_the_gate_agrees_to() {
        let port = service();
        let Some(agreed) = printed(Network::Proxy, true, &tunnel(port)).await else {
            return;
        };
        assert!(agreed.starts_with("HTTP/1.1 200"), "{agreed}");
        let refused = printed(Network::Proxy, false, &tunnel(port)).await.unwrap();
        assert!(refused.starts_with("HTTP/1.1 403"), "{refused}");
        let direct = printed(Network::Proxy, true, &direct(port)).await.unwrap();
        assert!(!direct.contains("connected"), "{direct}");
    }

    /// A command that is to go through the proxy with nobody to answer for it does not run.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_proxied_command_without_a_gate_is_refused() {
        let Ok(egress) = Egress::start(Arc::new(Default::default()), Default::default()).await
        else {
            return;
        };
        let sandbox = Sandbox {
            level: Level::Workspace,
            writable: Vec::new(),
            hidden: Vec::new(),
            network: Network::Proxy,
            egress: Some(Arc::new(egress)),
            unset: Vec::new(),
        };
        assert!(sandbox.command(None, "/bin/true", &[]).is_err());
    }
}
