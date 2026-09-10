//! The cheapest question a project can be asked about a change that was just written.
//!
//! A language server answers it best: it is already running, it knows the file, and it
//! replies in milliseconds. Not every project has one, and no project has one for every
//! file it holds, so there is a second answer underneath: the compiler, asked to compile
//! and nothing else.
//!
//! Narrow on purpose. The suite is the right thing to run when the work is done; after one
//! edit it is minutes spent to learn what a type check would have said in seconds, and the
//! model waits through all of it.

use std::path::Path;
use std::time::Duration;

/// How long the narrow check may take before it is abandoned.
///
/// A check that outlasts this is not the narrow one, and the model is better served by
/// carrying on than by waiting: whatever it would have said, the suite will say later.
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
    // Python has no project-wide compile step worth the name, so the question is asked of
    // the files that changed: a syntax error is what this catches, and that is what an
    // edit written blind gets wrong.
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

/// Ask, and hand back what went wrong when something did.
///
/// `None` means nothing to report: no check for this project, the check could not be run,
/// or it passed. A check that cannot run is not a failure of the change - saying so would
/// send the model to fix a compiler that is not installed.
pub async fn run(root: &Path, paths: &[String]) -> Option<String> {
    let (program, args, label) = narrowest(root, paths)?;
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(&args)
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(TIMEOUT, cmd.output())
        .await
        .ok()?
        .ok()?;
    if out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stderr);
    let text = if text.trim().is_empty() {
        String::from_utf8_lossy(&out.stdout).into_owned()
    } else {
        text.into_owned()
    };
    let text = text.trim();
    if text.is_empty() {
        return Some(format!("{label} failed"));
    }
    // The first lines carry the first error, which is the one to fix; the rest is usually
    // the same mistake seen from further away.
    let head: Vec<&str> = text.lines().take(24).collect();
    Some(format!("{label}:\n{}", head.join("\n")))
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

    /// Python has nothing project-wide worth running, so the question is asked of the
    /// files that changed and of nothing else.
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
        let said = run(dir.path(), &[])
            .await
            .expect("the crate does not build");
        assert!(said.starts_with("cargo check"), "{said}");
        assert!(said.contains("not_a_thing"), "{said}");
    }
}
