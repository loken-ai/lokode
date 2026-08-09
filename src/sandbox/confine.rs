//! Making the root a boundary for `bash` too.
//!
//! The path sandbox governs the tools that take paths. A shell takes a string, and once a
//! command is approved it runs with your privileges and your whole filesystem - which is
//! the largest gap between what verve enforces and what someone might assume it enforces.
//!
//! Linux can close part of it. Landlock lets a process give up access it will never need
//! again, and a child cannot get it back. What is given up here is *writing*: everything
//! outside the project, plus whatever the configuration names, becomes read-only for the
//! command and everything it starts. Reading is left alone on purpose - confining it too
//! would stop `cargo`, `npm` and every other tool from reaching a toolchain or a cache, and
//! a protection that makes the shell useless is one nobody will leave on.
//!
//! So this narrows the blast radius rather than sealing it: `rm -rf ~` stops working,
//! `cat ~/.ssh/id_rsa` does not. It is off unless asked for, because turning it on can make
//! a build fail in a way that looks like a bug in the build.

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

    // The write-shaped accesses, by the ABI that introduced each.
    const WRITE_FILE: u64 = 1 << 1;
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
    }

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
    /// with other threads can deadlock in the allocator. What is left for the child is two
    /// bare syscalls.
    pub fn ruleset(writable: &[PathBuf]) -> io::Result<OwnedFd> {
        let Some(abi) = abi() else {
            return Err(io::Error::other("this kernel has no Landlock"));
        };
        let attr = RulesetAttr {
            handled_access_fs: writes_for(abi),
        };
        // SAFETY: `attr` is live for the call and its size is what the kernel is told.
        let fd = unsafe {
            libc::syscall(
                SYS_CREATE_RULESET,
                &attr as *const RulesetAttr,
                std::mem::size_of::<RulesetAttr>(),
                0usize,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh descriptor the kernel just handed us, owned from here.
        let ruleset = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };

        for dir in writable {
            allow_beneath(&ruleset, dir, writes_for(abi))?;
        }
        Ok(ruleset)
    }

    fn allow_beneath(ruleset: &OwnedFd, dir: &Path, access: u64) -> io::Result<()> {
        let c_path = std::ffi::CString::new(dir.as_os_str().as_bytes())
            .map_err(|_| io::Error::other("path contains a null byte"))?;
        // SAFETY: a valid C string; O_PATH opens the directory as a reference only.
        let parent = unsafe { libc::open(c_path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if parent < 0 {
            // A directory that is not there cannot be written to either, so this is not
            // fatal - the point is what the command may reach, not what it was promised.
            tracing::warn!(path = %dir.display(), "not adding a landlock rule for a path that does not open");
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
    /// Two syscalls and nothing else: no allocation, no locks, nothing that another thread
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

    pub fn ruleset(_writable: &[PathBuf]) -> io::Result<OwnedFd> {
        Err(io::Error::other("shell confinement needs Linux"))
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

    /// Whether it is available is a fact about the machine, and the answer has to be a fact
    /// rather than a guess - the shell is confined or it is not.
    #[test]
    fn availability_is_reported_honestly() {
        match abi() {
            Some(v) => assert!(v >= 1, "an ABI of {v} is not a version"),
            None => {
                // Then building a ruleset must fail rather than silently confine nothing.
                assert!(ruleset(&[std::env::temp_dir()]).is_err());
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
        assert!(ruleset(&[dir.path().to_path_buf()]).is_ok());
    }

    /// A directory that is not there is not a reason to refuse to confine anything.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_missing_writable_path_is_skipped_rather_than_fatal() {
        if abi().is_none() {
            return;
        }
        assert!(ruleset(&[std::path::PathBuf::from("/no/such/directory/anywhere")]).is_ok());
    }
}
