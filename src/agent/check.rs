//! The cheapest question a project can be asked about a change that was just written.
//!
//! A language server answers it first.
//! It is already running, it knows the file, and it replies fast.
//! Not every project has one, and none has one for every file it holds.
//!
//! The compiler, asked to compile and nothing else, answers when there is no language server.
//!
//! The check is narrow.
//! The suite runs when the work is done, not after each edit.
//!
//! A Rust project is checked with the options of the last cargo command that succeeded in the
//! session, which say which features build here.
//! A check that fails for a reason outside the project's files, such as a dependency that cannot
//! build on this machine, says nothing about the change.
//! It is reported as no check, and not run again with the same options.

use std::path::Path;
use std::time::Duration;

use crate::lsp::check::Verdict;

/// How long the narrow check may take before it is abandoned.
///
/// A check that outlasts this is not the narrow one.
/// The suite reports whatever the check would have reported.
const TIMEOUT: Duration = Duration::from_secs(45);

/// What a session has learned about checking its project.
#[derive(Debug, Default)]
pub struct Memory {
    /// The options of the last cargo command that succeeded and chose what is built.
    cargo_options: Vec<String>,
    /// The check last found failing for a reason outside the project's files, as it was run.
    outside: Option<Vec<String>>,
}

impl Memory {
    /// Take the options of a command line that succeeded, when it runs cargo.
    pub fn learn(&mut self, command: &str) {
        if let Some(options) = cargo_options(command) {
            self.cargo_options = options;
        }
    }
}

/// The options of the last cargo invocation in `command` that choose what is built.
/// `None` when the command runs no cargo.
pub fn cargo_options(command: &str) -> Option<Vec<String>> {
    const SEPARATORS: [&str; 4] = ["&&", "||", ";", "|"];
    const FLAGS: [&str; 3] = ["--no-default-features", "--all-features", "--workspace"];
    const VALUED: [&str; 6] = [
        "--features",
        "-F",
        "--package",
        "-p",
        "--manifest-path",
        "--target",
    ];
    let words: Vec<&str> = command.split_whitespace().collect();
    let start = words
        .iter()
        .rposition(|w| *w == "cargo" || w.ends_with("/cargo"))?;
    let mut options = Vec::new();
    let mut rest = words[start + 1..]
        .iter()
        .map(|w| w.trim_end_matches(';'))
        .take_while(|w| !SEPARATORS.contains(w));
    while let Some(word) = rest.next() {
        if FLAGS.contains(&word) {
            options.push(word.to_string());
        } else if VALUED.contains(&word) {
            if let Some(value) = rest.next() {
                options.push(word.to_string());
                options.push(value.trim_matches(['"', '\'']).to_string());
            }
        } else if let Some((flag, _)) = word.split_once('=') {
            if VALUED.contains(&flag) {
                options.push(word.replace(['"', '\''], ""));
            }
        }
    }
    Some(options)
}

/// Whether `text` places something in a file of the project at `root`: a path that exists
/// under it, followed by a line number.
/// A failure that names no such place comes from outside the project's files.
fn names_a_project_place(root: &Path, text: &str) -> bool {
    without_escapes(text)
        .split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ','))
        .filter_map(|word| {
            let at = word.find([':', '('])?;
            let (path, place) = word.split_at(at);
            place[1..]
                .starts_with(|c: char| c.is_ascii_digit())
                .then_some(path)
        })
        .any(|path| {
            let path = Path::new(path.trim_start_matches("./"));
            let full = match path.is_absolute() {
                true => path.to_path_buf(),
                false => root.join(path),
            };
            full.starts_with(root) && full.is_file()
        })
}

/// The text without its terminal escape sequences.
///
/// A tool that colours its output puts escapes around the places it names.
fn without_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        if chars.next() == Some('[') {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        }
    }
    out
}

/// What to run to find out whether the project still builds, and what to call it.
fn narrowest(
    root: &Path,
    paths: &[String],
    cargo: &[String],
) -> Option<(&'static str, Vec<String>, &'static str)> {
    let owned = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    if root.join("Cargo.toml").exists() {
        let mut args = owned(&["check", "--quiet"]);
        args.extend(cargo.iter().cloned());
        return Some(("cargo", args, "cargo check"));
    }
    if root.join("tsconfig.json").exists() {
        return Some(("npx", owned(&["tsc", "--noEmit"]), "tsc"));
    }
    if root.join("go.mod").exists() {
        return Some(("go", owned(&["build", "./..."]), "go build"));
    }
    // Python has no project-wide compile step, so the check runs on the files that changed.
    // It catches syntax errors.
    let py: Vec<String> = paths
        .iter()
        .filter(|p| p.ends_with(".py"))
        .cloned()
        .collect();
    if !py.is_empty() {
        let mut args = owned(&["-m", "py_compile"]);
        args.extend(py);
        return Some(("python3", args, "py_compile"));
    }
    None
}

/// Ask, and say what came of it.
///
/// `None` when there is no check for this project or it could not be started.
/// A check that cannot run is not a failure of the change.
pub async fn run(
    root: &Path,
    paths: &[String],
    memory: &std::sync::Mutex<Memory>,
) -> Option<Verdict> {
    let cargo = memory
        .lock()
        .map(|m| m.cargo_options.clone())
        .unwrap_or_default();
    let (program, args, label) = narrowest(root, paths, &cargo)?;
    let outside = memory
        .lock()
        .map(|m| m.outside.as_ref() == Some(&args))
        .unwrap_or(false);
    if outside {
        return Some(Verdict::NotChecked {
            why: format!("{label} fails here for a reason outside the project's files"),
        });
    }
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(&args)
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = match tokio::time::timeout(TIMEOUT, cmd.output()).await {
        Err(_) => {
            return Some(Verdict::NotChecked {
                why: format!("{label} took longer than {}s", TIMEOUT.as_secs()),
            })
        }
        Ok(out) => out.ok()?,
    };
    if out.status.success() {
        if let Ok(mut m) = memory.lock() {
            m.outside = None;
        }
        return Some(Verdict::Passed {
            by: label.to_string(),
            notes: String::new(),
        });
    }
    let text = String::from_utf8_lossy(&out.stderr);
    let text = if text.trim().is_empty() {
        String::from_utf8_lossy(&out.stdout).into_owned()
    } else {
        text.into_owned()
    };
    let text = text.trim();
    if !text.is_empty() && !names_a_project_place(root, text) {
        if let Ok(mut m) = memory.lock() {
            m.outside = Some(args);
        }
        return Some(Verdict::NotChecked {
            why: format!(
                "{label} fails for a reason outside the project's files, so it says nothing \
                 about this change:\n{}",
                text.lines().take(12).collect::<Vec<_>>().join("\n")
            ),
        });
    }
    let report = match text.is_empty() {
        true => format!("{label} failed"),
        // The first lines carry the first error, which is the one to fix.
        // The rest is usually the same mistake seen from further away.
        false => format!(
            "{label}:\n{}",
            text.lines().take(24).collect::<Vec<_>>().join("\n")
        ),
    };
    Some(Verdict::Failed {
        by: label.to_string(),
        report,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rust_project_is_asked_to_compile_and_not_to_test() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        let (program, args, label) = narrowest(dir.path(), &[], &[]).unwrap();
        assert_eq!(program, "cargo");
        assert!(args.contains(&"check".to_string()), "{args:?}");
        assert!(!args.contains(&"test".to_string()), "{args:?}");
        assert_eq!(label, "cargo check");
    }

    /// Python has nothing project-wide worth running.
    /// The check runs on the files that changed and on nothing else.
    #[test]
    fn python_is_asked_about_the_files_that_changed() {
        let dir = tempfile::tempdir().unwrap();
        let paths = vec!["a.py".to_string(), "notes.md".to_string()];
        let (program, args, _) = narrowest(dir.path(), &paths, &[]).unwrap();
        assert_eq!(program, "python3");
        assert!(args.contains(&"a.py".to_string()), "{args:?}");
        assert!(!args.contains(&"notes.md".to_string()), "{args:?}");
    }

    #[test]
    fn a_project_with_nothing_to_ask_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        assert!(narrowest(dir.path(), &["a.txt".to_string()], &[]).is_none());
    }

    /// A place named in coloured output is still a place of the project.
    #[test]
    fn a_coloured_place_is_still_a_place() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
        let text = "\u{1b}[0m\u{1b}[1m\u{1b}[38;5;12m--> \u{1b}[0msrc/main.rs:1:13\u{1b}[0m";
        assert!(names_a_project_place(dir.path(), text));
    }

    #[tokio::test]
    async fn a_project_that_does_not_build_says_what_is_wrong() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/main.rs"),
            "fn main() { not_a_thing() }",
        )
        .unwrap();
        let memory = Default::default();
        let Some(Verdict::Failed { report, .. }) = run(dir.path(), &[], &memory).await else {
            panic!("the crate does not build");
        };
        assert!(report.starts_with("cargo check"), "{report}");
        assert!(report.contains("not_a_thing"), "{report}");
    }

    /// The options that choose what is built are taken from a cargo command line, and the rest
    /// is left.
    #[test]
    fn a_cargo_command_gives_the_options_that_choose_what_is_built() {
        assert_eq!(
            cargo_options(
                "cd x && cargo test -j 8 --no-default-features --features cpu --lib name && echo ok"
            ),
            Some(vec![
                "--no-default-features".to_string(),
                "--features".to_string(),
                "cpu".to_string()
            ])
        );
        assert_eq!(
            cargo_options("cargo +stable build --features='a,b' -p core"),
            Some(vec![
                "--features=a,b".to_string(),
                "-p".to_string(),
                "core".to_string()
            ])
        );
        assert_eq!(cargo_options("ls -la"), None);
    }

    /// A failure placed in a file of the project is the project's.
    /// One placed outside it, or nowhere, is not.
    #[test]
    fn a_failure_is_the_projects_when_it_names_a_place_in_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "").unwrap();
        assert!(names_a_project_place(
            root,
            "error[E0425]: cannot find value\n --> src/a.rs:3:5"
        ));
        assert!(names_a_project_place(root, "./src/a.rs(3,5): error TS2304"));
        assert!(!names_a_project_place(
            root,
            "error: failed to run custom build command for `cudarc v0.19.10`\n\
             thread 'main' panicked at /home/u/.cargo/registry/src/cudarc/build.rs:42:9:\n\
             unsupported CUDA version"
        ));
        assert!(!names_a_project_place(root, "error: could not compile"));
    }

    /// A dependency that cannot build here fails the check outside the project's files.
    /// That is no check of the change, and the same check is not run again.
    #[tokio::test]
    async fn a_dependency_that_cannot_build_here_is_not_the_changes_failure() {
        let dir = tempfile::tempdir().unwrap();
        let dep = dir.path().join("dep");
        let project = dir.path().join("project");
        for (at, name) in [(&dep, "dep"), (&project, "project")] {
            std::fs::create_dir_all(at.join("src")).unwrap();
            std::fs::write(at.join("src/lib.rs"), "").unwrap();
            let needs = match name {
                "project" => "[dependencies]\ndep = { path = \"../dep\" }\n",
                _ => "",
            };
            std::fs::write(
                at.join("Cargo.toml"),
                format!(
                    "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{needs}"
                ),
            )
            .unwrap();
        }
        std::fs::write(
            dep.join("build.rs"),
            "fn main() { panic!(\"this machine lacks what dep needs\") }",
        )
        .unwrap();
        let root = project.canonicalize().unwrap();
        let memory = Default::default();
        let Some(Verdict::NotChecked { why }) = run(&root, &[], &memory).await else {
            panic!("a failure outside the project was reported as the change's");
        };
        assert!(why.contains("outside the project's files"), "{why}");
        let Some(Verdict::NotChecked { why }) = run(&root, &[], &memory).await else {
            panic!("the check ran again");
        };
        assert!(why.contains("fails here"), "{why}");
    }
}
