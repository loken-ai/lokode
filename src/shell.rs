//! The shell that runs the commands of a session: the tool, the hooks and the `!` line.
//!
//! Unix has `/bin/sh`.
//! Windows has none, so the shell of Git for Windows is used when it is installed, and
//! PowerShell when it is not.
//! The tool tells the model which one it is writing for.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The language a shell reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Posix,
    PowerShell,
}

/// A program that runs a line of shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shell {
    pub program: PathBuf,
    pub dialect: Dialect,
}

impl Shell {
    /// The arguments that make the shell run `line`.
    ///
    /// A login shell reads the person's profile, which is where a version manager puts its
    /// tools on the path.
    pub fn args(&self, line: &str, login: bool) -> Vec<String> {
        match self.dialect {
            Dialect::Posix => vec![(if login { "-lc" } else { "-c" }).to_string(), line.into()],
            Dialect::PowerShell => ["-NoProfile", "-NonInteractive", "-Command", line]
                .map(String::from)
                .to_vec(),
        }
    }

    /// The shell of this machine, looked for once.
    pub fn current() -> &'static Shell {
        static CURRENT: OnceLock<Shell> = OnceLock::new();
        CURRENT.get_or_init(|| {
            if cfg!(windows) {
                find_on_windows(&|name| std::env::var_os(name).map(PathBuf::from), &|p| {
                    p.is_file()
                })
            } else {
                Shell {
                    program: PathBuf::from("/bin/sh"),
                    dialect: Dialect::Posix,
                }
            }
        })
    }
}

/// The shell of a Windows machine, given how to read its environment and its files.
///
/// Git for Windows is looked for where it installs, then by the `git` on the path.
/// PowerShell 7 comes before the one every Windows has.
fn find_on_windows(
    env: &dyn Fn(&str) -> Option<PathBuf>,
    is_file: &dyn Fn(&Path) -> bool,
) -> Shell {
    let mut git_sh = Vec::new();
    for var in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
        if let Some(dir) = env(var) {
            git_sh.push(dir.join("Git").join("bin").join("sh.exe"));
        }
    }
    if let Some(dir) = env("LOCALAPPDATA") {
        git_sh.push(dir.join("Programs").join("Git").join("bin").join("sh.exe"));
    }
    let path = env("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        // `git.exe` sits in `cmd` or `bin` of the installation, whose `bin` holds `sh.exe`.
        if is_file(&dir.join("git.exe")) {
            if let Some(root) = dir.parent() {
                git_sh.push(root.join("bin").join("sh.exe"));
            }
        }
    }
    if let Some(found) = git_sh.into_iter().find(|p| is_file(p)) {
        return Shell {
            program: found,
            dialect: Dialect::Posix,
        };
    }
    for dir in std::env::split_paths(&path) {
        let pwsh = dir.join("pwsh.exe");
        if is_file(&pwsh) {
            return Shell {
                program: pwsh,
                dialect: Dialect::PowerShell,
            };
        }
    }
    let root = env("SystemRoot").unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    Shell {
        program: root
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe"),
        dialect: Dialect::PowerShell,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn machine(vars: &[(&str, &str)], files: &[&str]) -> Shell {
        let files: HashSet<PathBuf> = files.iter().map(PathBuf::from).collect();
        find_on_windows(
            &|name| {
                vars.iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| PathBuf::from(v))
            },
            &|p| files.contains(p),
        )
    }

    #[test]
    fn a_unix_shell_runs_a_line_as_a_login_shell_or_not() {
        let sh = Shell {
            program: "/bin/sh".into(),
            dialect: Dialect::Posix,
        };
        assert_eq!(sh.args("ls", true), ["-lc", "ls"]);
        assert_eq!(sh.args("ls", false), ["-c", "ls"]);
    }

    #[test]
    fn powershell_runs_a_line_without_a_profile() {
        let ps = Shell {
            program: "powershell.exe".into(),
            dialect: Dialect::PowerShell,
        };
        assert_eq!(
            ps.args("dir", true),
            ["-NoProfile", "-NonInteractive", "-Command", "dir"]
        );
    }

    #[test]
    fn git_for_windows_is_found_where_it_installs() {
        let sh = machine(
            &[("ProgramFiles", r"C:\Program Files")],
            &[&PathBuf::from(r"C:\Program Files")
                .join("Git")
                .join("bin")
                .join("sh.exe")
                .to_string_lossy()],
        );
        assert_eq!(sh.dialect, Dialect::Posix);
        assert!(sh.program.ends_with("sh.exe"));
    }

    #[test]
    fn git_is_found_by_the_git_on_the_path() {
        let cmd = PathBuf::from("/tools/git/cmd");
        let sh = machine(
            &[("PATH", &cmd.to_string_lossy())],
            &[
                &cmd.join("git.exe").to_string_lossy(),
                &PathBuf::from("/tools/git")
                    .join("bin")
                    .join("sh.exe")
                    .to_string_lossy(),
            ],
        );
        assert_eq!(sh.dialect, Dialect::Posix);
    }

    #[test]
    fn powershell_is_the_fallback_and_version_7_comes_first() {
        let none = machine(&[("SystemRoot", r"C:\Windows")], &[]);
        assert_eq!(none.dialect, Dialect::PowerShell);
        assert!(none.program.ends_with("powershell.exe"));
        let dir = PathBuf::from("/tools/ps7");
        let seven = machine(
            &[("PATH", &dir.to_string_lossy())],
            &[&dir.join("pwsh.exe").to_string_lossy()],
        );
        assert!(seven.program.ends_with("pwsh.exe"));
    }
}
