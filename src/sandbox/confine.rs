//! Making the root a boundary for the commands the agent runs.
//!
//! The path sandbox governs the tools that take paths. A shell takes a string, and an
//! approved command runs with the user's privileges. A confined command and everything it
//! starts may write only beneath the directories its [`Sandbox`] names, and to the devices
//! under `/dev`. It may read everything but what the sandbox hides, and open TCP
//! connections only when the sandbox lets it.
//!
//! | System | Mechanism |
//! |---|---|
//! | Linux | Landlock: the command gives up the access before it runs, for good |
//! | macOS | Seatbelt: the command runs under `sandbox-exec` with a profile written here |

use std::path::PathBuf;

/// What has to be kept until a confined command has been spawned.
pub type Held = Option<std::os::fd::OwnedFd>;

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

/// A command that runs `program` with `args`, in `sandbox` when there is one, and what
/// must be kept until it has been spawned. A sandbox that cannot be applied is an error,
/// never a command left unconfined.
pub fn command(
    sandbox: Option<&Sandbox>,
    program: &str,
    args: &[&str],
) -> std::io::Result<(tokio::process::Command, Held)> {
    match sandbox {
        Some(s) => s.command(program, args),
        None => {
            let mut cmd = tokio::process::Command::new(program);
            cmd.args(args);
            Ok((cmd, None))
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

/// What a confined command may do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sandbox {
    pub level: Level,
    /// The directories and files it may write beneath, besides the devices.
    pub writable: Vec<PathBuf>,
    /// The directories and files it may not read.
    pub hidden: Vec<PathBuf>,
    /// Whether it may bind and connect TCP sockets.
    pub network: bool,
    /// The environment variables taken out of what it inherits.
    pub unset: Vec<String>,
}

impl Sandbox {
    /// A command that runs `program` with `args` in this sandbox, and what must be kept
    /// until it has been spawned.
    pub fn command(
        &self,
        program: &str,
        args: &[&str],
    ) -> std::io::Result<(tokio::process::Command, Held)> {
        if cfg!(target_os = "macos") {
            let mut cmd = seatbelt::command(&seatbelt::profile(self), program, args);
            for var in &self.unset {
                cmd.env_remove(var);
            }
            return Ok((cmd, None));
        }
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args);
        let fd = self.confine(&mut cmd)?;
        Ok((cmd, Some(fd)))
    }

    /// Confine `cmd` with Landlock: take out the variables it must not see, and have it
    /// give up what the sandbox does not allow before it runs. The descriptor returned
    /// must outlive the spawn.
    fn confine(&self, cmd: &mut tokio::process::Command) -> std::io::Result<std::os::fd::OwnedFd> {
        for var in &self.unset {
            cmd.env_remove(var);
        }
        let fd = ruleset(&self.writable, &self.hidden, self.network)?;
        use std::os::fd::AsRawFd;
        let raw = fd.as_raw_fd();
        // SAFETY: `restrict_after_fork` makes 2 syscalls and nothing else, no allocation
        // and no locks, which is what a post-fork hook may do. `raw` stays open because
        // the caller keeps `fd` until the spawn is over.
        unsafe {
            cmd.pre_exec(move || restrict_after_fork(raw));
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
            true => "",
            false => ", and may not open a network connection",
        };
        format!(
            "It ran in a sandbox: it {writes}{network}. If the sandbox is what stopped it, the \
             same call with outside_sandbox set asks the user to run it unconfined."
        )
    }
}

/// The macOS sandbox: a profile in Seatbelt's language, and the command that runs under it.
///
/// Built on every system, so what it writes is tested wherever the tests run. The profile
/// allows everything the Linux sandbox does not govern, then takes away what it does: writes
/// outside the writable paths and the devices, reads of what is hidden, and TCP.
mod seatbelt {
    use std::path::{Path, PathBuf};

    pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

    /// The path as the system names it: `/tmp` is `/private/tmp` there, and a profile
    /// matches the real path.
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

    /// The profile for `sandbox`. The last rule that matches decides.
    pub fn profile(sandbox: &super::Sandbox) -> String {
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
        if !sandbox.network {
            out.push("(deny network-outbound (remote ip))".to_string());
            out.push("(deny network-bind (local ip))".to_string());
            out.push("(deny network-inbound (local ip))".to_string());
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
        // SAFETY: the documented way to ask - a null attribute with the version flag.
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

    /// Everything write-shaped this kernel knows how to govern.
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
        bits
    }

    /// Build the ruleset in the parent.
    ///
    /// Deliberately all of it: creating the ruleset and adding a rule per allowed directory
    /// both allocate and open files, and doing that between `fork` and `exec` in a process
    /// with other threads can deadlock in the allocator. What is left for the child is 2
    /// bare syscalls.
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
        // Writing to a device is how a command discards output or talks to a terminal, and
        // a device cannot be made or removed under this rule.
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

    /// What may be read beneath `dir` with `hidden` kept out: each path, and whether all
    /// of it may be read or only the names in it. A directory with something hidden below
    /// it has its names readable and is walked into. Names stay readable all the way down,
    /// contents of hidden paths do not.
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
        // SAFETY: a valid C string; O_PATH opens the directory as a reference only.
        let parent = unsafe { libc::open(c_path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if parent < 0 {
            // A path that does not open cannot be reached through this rule either, so
            // this is not fatal: the point is what the command may reach.
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

    /// Apply the ruleset to this process. Runs in the child, between `fork` and `exec`.
    ///
    /// 2 syscalls and nothing else: no allocation, no locks, nothing that another thread
    /// in the parent could have been holding when the fork happened.
    ///
    /// # Safety
    /// `fd` must be an open Landlock ruleset descriptor.
    pub unsafe fn restrict_after_fork(fd: RawFd) -> io::Result<()> {
        // Landlock refuses to apply without this, which is what stops a confined command
        // from regaining privileges through a setuid binary.
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
    pub unsafe fn restrict_after_fork(_fd: RawFd) -> io::Result<()> {
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
            network: false,
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
        let open = seatbelt::profile(&Sandbox {
            network: true,
            ..sandbox
        });
        assert!(!open.contains("network"), "{open}");
        let cmd = seatbelt::command(&profile, "/bin/sh", &["-lc", "ls"]);
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(cmd.as_std().get_program(), seatbelt::SANDBOX_EXEC);
        assert_eq!(args, ["-p", profile.as_str(), "--", "/bin/sh", "-lc", "ls"]);
    }

    /// Whether it is available is a fact about the machine, and the answer has to be a fact
    /// rather than a guess - the shell is confined or it is not.
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

    /// A hidden path is left out, the directories above it keep their names readable,
    /// and everything beside them is readable whole.
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
            network: true,
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
        let run = |network: bool| {
            let sandbox = Sandbox {
                level: Level::Workspace,
                writable: Vec::new(),
                hidden: Vec::new(),
                network,
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
        assert!(run(true).await);
        assert!(!run(false).await);
    }
}
