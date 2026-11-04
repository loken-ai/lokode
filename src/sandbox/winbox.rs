//! The Windows sandbox: a command runs at the low integrity level, in a job of its own.
//!
//! A low integrity process cannot write to anything that has a higher label, which is every file
//! of the person by default.
//! The directories a command may write to are given the low label first.
//! The job ends the command and what it started when `lokode confine-exec` ends.
//!
//! What it does not do: it does not read-protect the paths in `hidden`, and it does not cut the
//! network.
//!
//! `lokode confine-exec` is what the session runs in front of the shell.
//! It is a program of its own because a child's token is chosen when the child is created, and
//! `tokio::process::Command` creates it with the caller's.

/// A command line for `CreateProcess`: the program and its arguments, each quoted so that the
/// program splits them back into the same words.
pub fn command_line(program: &str, args: &[String]) -> String {
    let mut line = quote(program);
    for arg in args {
        line.push(' ');
        line.push_str(&quote(arg));
    }
    line
}

/// One word of a command line, in the rules of `CommandLineToArgvW`.
///
/// A run of backslashes is doubled when a quote follows it, and the quotes inside are escaped.
fn quote(word: &str) -> String {
    if !word.is_empty() && !word.contains([' ', '\t', '\n', '\x0b', '"']) {
        return word.to_string();
    }
    let mut out = String::from("\"");
    let mut slashes = 0;
    for c in word.chars() {
        match c {
            '\\' => slashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(slashes * 2 + 1));
                out.push('"');
                slashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(slashes));
                out.push(c);
                slashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(slashes * 2));
    out.push('"');
    out
}

#[cfg(windows)]
pub use imp::run;

#[cfg(windows)]
mod imp {
    use std::io;
    use std::path::{Path, PathBuf};
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Foundation::SetHandleInformation;
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, LocalFree, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
        WAIT_FAILED,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, ConvertStringSidToSidW,
        GetNamedSecurityInfoW, SetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{
        CreateRestrictedToken, GetSecurityDescriptorSacl, SetTokenInformation, TokenIntegrityLevel,
        ACL, DISABLE_MAX_PRIVILEGE, LABEL_SECURITY_INFORMATION, TOKEN_ADJUST_DEFAULT,
        TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{
        CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, OpenProcessToken,
        ResumeThread, WaitForSingleObject, CREATE_SUSPENDED, INFINITE, PROCESS_INFORMATION,
        STARTF_USESTDHANDLES, STARTUPINFOW,
    };

    /// The low integrity level, and the label that gives it to a directory and what is in it.
    const LOW_SID: &str = "S-1-16-4096";
    const LOW_LABEL: &str = "S:(ML;OICI;NW;;;LW)";
    /// The attribute that marks a group of a token as its integrity level.
    const SE_GROUP_INTEGRITY: u32 = 0x20;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }

    fn last_error(what: &str) -> io::Error {
        // SAFETY: reads the calling thread's last error.
        let code = unsafe { GetLastError() };
        io::Error::other(format!("{what} failed with Windows error {code}"))
    }

    /// A handle that is closed when it goes out of scope.
    struct Owned(HANDLE);

    impl Drop for Owned {
        fn drop(&mut self) {
            if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
                // SAFETY: a handle this value owns and has not closed.
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    /// The token of this process, cut down to the low integrity level and stripped of its
    /// privileges.
    fn low_token() -> io::Result<Owned> {
        let mut own: HANDLE = null_mut();
        // SAFETY: a token handle is written to `own`, which is closed below.
        let opened = unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_DEFAULT,
                &mut own,
            )
        };
        if opened == 0 {
            return Err(last_error("OpenProcessToken"));
        }
        let own = Owned(own);
        let mut restricted: HANDLE = null_mut();
        // SAFETY: `own` is a live token and `restricted` receives a new one.
        let made = unsafe {
            CreateRestrictedToken(
                own.0,
                DISABLE_MAX_PRIVILEGE,
                0,
                null(),
                0,
                null(),
                0,
                null(),
                &mut restricted,
            )
        };
        if made == 0 {
            return Err(last_error("CreateRestrictedToken"));
        }
        let restricted = Owned(restricted);

        let mut sid = null_mut();
        let name = wide(LOW_SID);
        // SAFETY: `name` is a NUL-terminated string, and `sid` is freed with `LocalFree`.
        if unsafe { ConvertStringSidToSidW(name.as_ptr(), &mut sid) } == 0 {
            return Err(last_error("ConvertStringSidToSidW"));
        }
        let label = TOKEN_MANDATORY_LABEL {
            Label: windows_sys::Win32::Security::SID_AND_ATTRIBUTES {
                Sid: sid,
                Attributes: SE_GROUP_INTEGRITY,
            },
        };
        // SAFETY: `label` and the SID it points to live for the call.
        let set = unsafe {
            SetTokenInformation(
                restricted.0,
                TokenIntegrityLevel,
                &label as *const _ as *const _,
                std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32,
            )
        };
        let failure = (set == 0).then(|| last_error("SetTokenInformation"));
        // SAFETY: the SID was allocated by `ConvertStringSidToSidW`.
        unsafe { LocalFree(sid as _) };
        match failure {
            Some(e) => Err(e),
            None => Ok(restricted),
        }
    }

    /// Give `dir` and what is in it the low label, unless it has a label already.
    fn allow_writes(dir: &Path) -> io::Result<()> {
        let path = wide(&dir.to_string_lossy());
        let mut sacl: *mut ACL = null_mut();
        let mut found = null_mut();
        // SAFETY: the security descriptor written to `found` is freed with `LocalFree`.
        let read = unsafe {
            GetNamedSecurityInfoW(
                path.as_ptr(),
                SE_FILE_OBJECT,
                LABEL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                null_mut(),
                &mut sacl,
                &mut found,
            )
        };
        if read != 0 {
            return Err(io::Error::from_raw_os_error(read as i32));
        }
        // SAFETY: `sacl` points into `found` when it is not null.
        let labelled = !sacl.is_null() && unsafe { (*sacl).AceCount } > 0;
        // SAFETY: allocated by `GetNamedSecurityInfoW`.
        unsafe { LocalFree(found as _) };
        if labelled {
            return Ok(());
        }

        let text = wide(LOW_LABEL);
        let mut descriptor = null_mut();
        // SAFETY: `text` is NUL-terminated, and the descriptor is freed with `LocalFree`.
        let parsed = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            )
        };
        if parsed == 0 {
            return Err(last_error(
                "ConvertStringSecurityDescriptorToSecurityDescriptorW",
            ));
        }
        let (mut present, mut defaulted) = (0, 0);
        let mut label_acl: *mut ACL = null_mut();
        // SAFETY: `descriptor` is a live descriptor and the out values are written.
        let got = unsafe {
            GetSecurityDescriptorSacl(descriptor, &mut present, &mut label_acl, &mut defaulted)
        };
        let result = if got == 0 || present == 0 {
            Err(last_error("GetSecurityDescriptorSacl"))
        } else {
            // SAFETY: `label_acl` points into `descriptor`, which outlives the call.
            let set = unsafe {
                SetNamedSecurityInfoW(
                    path.as_ptr() as *mut u16,
                    SE_FILE_OBJECT,
                    LABEL_SECURITY_INFORMATION,
                    null_mut(),
                    null_mut(),
                    null_mut(),
                    label_acl,
                )
            };
            match set {
                0 => Ok(()),
                code => Err(io::Error::from_raw_os_error(code as i32)),
            }
        };
        // SAFETY: allocated by the conversion above.
        unsafe { LocalFree(descriptor as _) };
        result
    }

    /// A job that ends every process in it when its last handle is closed.
    fn job() -> io::Result<Owned> {
        // SAFETY: an unnamed job with default security.
        let job = unsafe { CreateJobObjectW(null(), null()) };
        if job.is_null() {
            return Err(last_error("CreateJobObjectW"));
        }
        let job = Owned(job);
        // SAFETY: a plain structure of integers and pointers, all zero.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `limits` lives for the call and the size is its own.
        let set = unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        match set {
            0 => Err(last_error("SetInformationJobObject")),
            _ => Ok(job),
        }
    }

    /// A standard handle of this process, made inheritable.
    fn inheritable(which: u32) -> HANDLE {
        // SAFETY: asks for the process's own standard handle.
        let handle = unsafe { GetStdHandle(which) };
        if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
            // SAFETY: a live handle of this process.
            unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
        }
        handle
    }

    /// Run `program` with `args` at the low integrity level, and return its exit code.
    ///
    /// Each of `writable` that exists is given the low label first, and the ones that cannot be
    /// are said on the error output.
    pub fn run(writable: &[PathBuf], program: &str, args: &[String]) -> io::Result<i32> {
        for dir in writable.iter().filter(|d| d.exists()) {
            if let Err(e) = allow_writes(dir) {
                eprintln!("lokode: a command cannot write to {}: {e}", dir.display());
            }
        }
        let token = low_token()?;
        let job = job()?;

        // SAFETY: a plain structure of integers and pointers, all zero.
        let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
        startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        startup.dwFlags = STARTF_USESTDHANDLES;
        startup.hStdInput = inheritable(STD_INPUT_HANDLE);
        startup.hStdOutput = inheritable(STD_OUTPUT_HANDLE);
        startup.hStdError = inheritable(STD_ERROR_HANDLE);
        // SAFETY: a plain structure written by the call.
        let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let mut line = wide(&super::command_line(program, args));
        // SAFETY: `line` is a NUL-terminated buffer the call may write to, and the structures
        // live for the call.
        let created = unsafe {
            CreateProcessAsUserW(
                token.0,
                null(),
                line.as_mut_ptr(),
                null(),
                null(),
                1,
                CREATE_SUSPENDED,
                null(),
                null(),
                &startup,
                &mut process,
            )
        };
        if created == 0 {
            return Err(last_error("CreateProcessAsUserW"));
        }
        let (child, thread) = (Owned(process.hProcess), Owned(process.hThread));
        // The process is in the job before it runs a single instruction.
        // SAFETY: both handles are live.
        if unsafe { AssignProcessToJobObject(job.0, child.0) } == 0 {
            return Err(last_error("AssignProcessToJobObject"));
        }
        // SAFETY: the suspended thread of the child just created.
        unsafe { ResumeThread(thread.0) };
        // SAFETY: `child` is a live process handle.
        if unsafe { WaitForSingleObject(child.0, INFINITE) } == WAIT_FAILED {
            return Err(last_error("WaitForSingleObject"));
        }
        let mut code = 0u32;
        // SAFETY: `child` is a process handle that has ended.
        if unsafe { GetExitCodeProcess(child.0, &mut code) } == 0 {
            return Err(last_error("GetExitCodeProcess"));
        }
        Ok(code as i32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_plain_word_is_left_alone() {
        assert_eq!(
            command_line("sh.exe", &words(&["-lc", "ls"])),
            "sh.exe -lc ls"
        );
    }

    #[test]
    fn a_word_with_a_space_or_a_quote_is_quoted() {
        assert_eq!(quote("a b"), "\"a b\"");
        assert_eq!(quote(""), "\"\"");
        assert_eq!(quote("say \"hi\""), "\"say \\\"hi\\\"\"");
    }

    #[test]
    fn backslashes_are_doubled_only_before_a_quote() {
        assert_eq!(quote("C:\\dir\\file"), "C:\\dir\\file");
        assert_eq!(quote("C:\\my dir\\"), "\"C:\\my dir\\\\\"");
        assert_eq!(quote("a\\\"b"), "\"a\\\\\\\"b\"");
    }
}
