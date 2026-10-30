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

use std::path::Path;
use std::time::Duration;

use crate::lsp::check::Verdict;

/// How long the narrow check may take before it is abandoned.
///
/// A check that outlasts this is not the narrow one.
/// The suite reports whatever the check would have reported.
const TIMEOUT: Duration = Duration::from_secs(45);

/// What to run to find out whether the project still builds, and what to call it.
fn narrowest(root: &Path, paths: &[String]) -> Option<(&'static str, Vec<String>, &'static str)> {
    let owned = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    if root.join("Cargo.toml").exists() {
        return Some(("cargo", owned(&["check", "--quiet"]), "cargo check"));
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
pub async fn run(root: &Path, paths: &[String]) -> Option<Verdict> {
    let (program, args, label) = narrowest(root, paths)?;
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
        let (program, args, label) = narrowest(dir.path(), &[]).unwrap();
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
        let (program, args, _) = narrowest(dir.path(), &paths).unwrap();
        assert_eq!(program, "python3");
        assert!(args.contains(&"a.py".to_string()), "{args:?}");
        assert!(!args.contains(&"notes.md".to_string()), "{args:?}");
    }

    #[test]
    fn a_project_with_nothing_to_ask_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        assert!(narrowest(dir.path(), &["a.txt".to_string()]).is_none());
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
        let Some(Verdict::Failed { report, .. }) = run(dir.path(), &[]).await else {
            panic!("the crate does not build");
        };
        assert!(report.starts_with("cargo check"), "{report}");
        assert!(report.contains("not_a_thing"), "{report}");
    }
}
