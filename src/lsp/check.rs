//! What a write did to what the language servers report.
//!
//! The servers are told of the written files as changed and saved.
//! That starts a server's own build where it has one.
//!
//! Once the servers have published about the new text and have no work left in hand,
//! what they report is compared with what they reported before.
//! Only what the write introduced is news, in the written files and in any other.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Url};
use serde_json::json;

use crate::lsp::position;
use crate::lsp::server::{Published, Server};
use crate::lsp::LspPool;

/// Opens what a check that found errors adds to a write's result.
///
/// The model reads it.
/// The step record reads it to know the check failed.
/// The screen marks the step.
pub const CHECK_FAILED: &str = "the language server reports:";

/// Opens what a check that found no new error adds to a write's result.
pub const CHECK_PASSED: &str = "checked by";

/// Opens what a check that could not tell adds to a write's result.
pub const NOT_CHECKED: &str = "not checked:";

/// What came of the check of a write, as its result says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    Passed,
    Failed,
    NotChecked,
}

/// The check a write's result reports, with what it said from its opening on.
pub fn read(output: &str) -> Option<(Outcome, &str)> {
    [
        (CHECK_FAILED, Outcome::Failed),
        (NOT_CHECKED, Outcome::NotChecked),
        (CHECK_PASSED, Outcome::Passed),
    ]
    .into_iter()
    .find_map(|(marker, outcome)| {
        let at = output.find(&format!("\n\n{marker}"))?;
        Some((outcome, output[at + 2..].trim_end()))
    })
}

/// How long the servers must stay silent before what they published is taken as settled.
const QUIET: Duration = Duration::from_millis(400);

/// How long a server that reports no work in hand has to publish about a file.
const FIRST_WORD: Duration = Duration::from_secs(3);

/// How long a server that reports work in hand is waited for.
pub const SETTLE_AT_MOST: Duration = Duration::from_secs(20);

/// How often a waiter looks again when nothing wakes it.
const POLL: Duration = Duration::from_millis(100);

/// How many diagnostics a report lists before it counts the rest.
const SHOWN: usize = 12;

/// What the language servers say about a write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// They settled and report no new error. `notes` lists what else they report.
    Passed { by: String, notes: String },
    /// They report errors the write introduced.
    Failed { by: String, report: String },
    /// They could not say: none was running, one stopped, or one was still working.
    NotChecked { why: String },
}

/// Tell the servers of `paths` and say what the write did. `None` when no server answers
/// for any of them.
pub async fn after_write(pool: &LspPool, root: &Path, paths: &[PathBuf]) -> Option<Verdict> {
    let mut by_server: Vec<(Arc<Server>, Vec<PathBuf>)> = Vec::new();
    let mut down = Vec::new();
    for path in paths.iter().filter(|p| p.exists()) {
        if !pool.knows(path) {
            continue;
        }
        match pool.server_for(path).await {
            Ok(server) => match by_server.iter_mut().find(|(s, _)| Arc::ptr_eq(s, &server)) {
                Some((_, list)) => list.push(path.clone()),
                None => by_server.push((server, vec![path.clone()])),
            },
            Err(why) => down.push(why),
        }
    }
    if by_server.is_empty() {
        return down
            .into_iter()
            .next()
            .map(|why| Verdict::NotChecked { why });
    }
    let mut failed = Vec::new();
    let mut passed = Vec::new();
    let mut unchecked = down;
    for (server, files) in by_server {
        match check_one(&server, root, &files).await {
            Verdict::Failed { report, .. } => failed.push(report),
            Verdict::Passed { by, notes } => passed.push((by, notes)),
            Verdict::NotChecked { why } => unchecked.push(why),
        }
    }
    let by = passed
        .iter()
        .map(|(by, _)| by.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    if !failed.is_empty() {
        return Some(Verdict::Failed {
            by,
            report: failed.join("\n"),
        });
    }
    if !unchecked.is_empty() {
        return Some(Verdict::NotChecked {
            why: unchecked.join("; "),
        });
    }
    let notes = passed
        .into_iter()
        .map(|(_, n)| n)
        .filter(|n| !n.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    Some(Verdict::Passed { by, notes })
}

async fn check_one(server: &Server, root: &Path, files: &[PathBuf]) -> Verdict {
    let by = server.spec.command.clone();
    let before = server.all_published().await;
    let asked = Instant::now();
    let mut targets = Vec::new();
    for path in files {
        match server.sync(path).await {
            Ok((uri, version, sent)) => {
                if server.caps().save {
                    let _ = server
                        .notify(
                            "textDocument/didSave",
                            json!({"textDocument": {"uri": uri}}),
                        )
                        .await;
                }
                targets.push((uri, version, sent));
            }
            Err(e) => {
                return Verdict::NotChecked {
                    why: format!("{by} could not be told of {}: {e:#}", path.display()),
                }
            }
        }
    }
    if let Err(why) = settle(server, &targets, asked, SETTLE_AT_MOST).await {
        return Verdict::NotChecked { why };
    }
    let after = server.all_published().await;
    let written: Vec<&Url> = targets.iter().map(|(u, _, _)| u).collect();
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let mut kept_errors = 0usize;
    for (uri, now) in &after {
        let is_written = written.contains(&uri);
        if !is_written && now.at < asked {
            continue;
        }
        let was = before
            .get(uri)
            .map(|p| p.diagnostics.as_slice())
            .unwrap_or(&[]);
        let (new, kept) = split_new(was, &now.diagnostics);
        for d in new {
            match d.severity {
                Some(DiagnosticSeverity::ERROR) => errors.push((uri.clone(), d)),
                Some(DiagnosticSeverity::WARNING) => warnings.push((uri.clone(), d)),
                _ => {}
            }
        }
        if is_written {
            kept_errors += kept
                .iter()
                .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
                .count();
        }
    }
    let mut lines = Vec::new();
    let errors = first_on_each_line(&errors);
    let warnings = first_on_each_line(&warnings);
    for (uri, d) in errors.iter().chain(warnings.iter()).take(SHOWN) {
        lines.push(describe(server, root, uri, d).await);
    }
    let listed = lines.len();
    let rest = errors.len() + warnings.len() - listed;
    if rest > 0 {
        lines.push(format!("and {rest} more"));
    }
    if kept_errors > 0 {
        lines.push(format!(
            "{kept_errors} error(s) in the written files were there before this write"
        ));
    }
    let body = lines.join("\n");
    if errors.is_empty() {
        let notes = match warnings.is_empty() && kept_errors == 0 {
            true => String::new(),
            false => format!("{by}: {} new warning(s)\n{body}", warnings.len()),
        };
        return Verdict::Passed { by, notes };
    }
    Verdict::Failed {
        by: by.clone(),
        report: format!(
            "{by}: {} new error(s), {} new warning(s)\n{body}",
            errors.len(),
            warnings.len()
        ),
    }
}

/// Wait until `server` has published about each of `targets` as last sent.
///
/// The server must also have no work in hand and have been silent for a moment.
/// Says why when it does not within `at_most`.
pub async fn settle(
    server: &Server,
    targets: &[(Url, i32, bool)],
    asked: Instant,
    at_most: Duration,
) -> Result<(), String> {
    loop {
        if !server.alive() {
            return Err(format!(
                "{} stopped while checking{}",
                server.spec.command,
                server.stderr_tail().await
            ));
        }
        let published = server.all_published().await;
        let current = targets
            .iter()
            .all(|(uri, version, sent)| is_current(published.get(uri), *version, *sent, asked));
        let busy = server.busy().await;
        let last = published
            .values()
            .map(|p| p.at)
            .max()
            .unwrap_or(asked)
            .max(asked);
        let now = Instant::now();
        if current && !busy && now.duration_since(last) >= QUIET {
            return Ok(());
        }
        if !busy && !current && now.duration_since(asked) >= FIRST_WORD {
            return Err(format!(
                "{} said nothing about the file",
                server.spec.command
            ));
        }
        if now.duration_since(asked) >= at_most {
            let doing = server
                .progress()
                .await
                .into_iter()
                .next()
                .map(|p| p.title)
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| "working through the project".into());
            return Err(format!(
                "{} is still {}",
                server.spec.command,
                doing.to_lowercase()
            ));
        }
        let _ = tokio::time::timeout(POLL, server.changed.notified()).await;
    }
}

/// Whether `published` describes the text sent at `version`.
fn is_current(published: Option<&Published>, version: i32, sent: bool, asked: Instant) -> bool {
    match published {
        None => false,
        Some(p) => match (p.version, sent) {
            (Some(v), _) => v >= version,
            (None, true) => p.at >= asked,
            (None, false) => true,
        },
    }
}

/// What a diagnostic is, regardless of where it sits.
fn key(d: &Diagnostic) -> (Option<String>, Option<String>, String) {
    (
        d.severity.map(|s| format!("{s:?}")),
        code(d),
        d.message.clone(),
    )
}

/// The diagnostics of `now` that `was` did not have, and those it did.
fn split_new<'a>(
    was: &[Diagnostic],
    now: &'a [Diagnostic],
) -> (Vec<&'a Diagnostic>, Vec<&'a Diagnostic>) {
    let mut left: HashMap<_, usize> = HashMap::new();
    for d in was {
        *left.entry(key(d)).or_default() += 1;
    }
    let mut new = Vec::new();
    let mut kept = Vec::new();
    for d in now {
        match left.get_mut(&key(d)) {
            Some(n) if *n > 0 => {
                *n -= 1;
                kept.push(d);
            }
            _ => new.push(d),
        }
    }
    (new, kept)
}

fn code(d: &Diagnostic) -> Option<String> {
    d.code.as_ref().map(|c| match c {
        NumberOrString::Number(n) => n.to_string(),
        NumberOrString::String(s) => s.clone(),
    })
}

/// The first of the diagnostics that share a file, a start line and what they say.
///
/// A server can report one problem once per column it spans: that is one problem.
fn first_on_each_line<'a>(list: &[(Url, &'a Diagnostic)]) -> Vec<(Url, &'a Diagnostic)> {
    let mut seen = std::collections::HashSet::new();
    list.iter()
        .filter(|(uri, d)| seen.insert((uri.clone(), d.range.start.line, key(d))))
        .cloned()
        .collect()
}

/// One diagnostic as a line a model can act on.
///
/// The line holds where it is in characters, its severity and code, what it says,
/// and the source line under it.
pub async fn describe(server: &Server, root: &Path, uri: &Url, d: &Diagnostic) -> String {
    let path = uri.to_file_path().unwrap_or_default();
    let shown = path
        .strip_prefix(root)
        .unwrap_or(&path)
        .display()
        .to_string();
    let text = server.text_of(&path).await.unwrap_or_default();
    let line = d.range.start.line as usize;
    let source = text.lines().nth(line).unwrap_or("");
    let column = position::chars(source, d.range.start.character, server.caps().encoding) + 1;
    let severity = match d.severity {
        Some(DiagnosticSeverity::ERROR) => "error",
        Some(DiagnosticSeverity::WARNING) => "warning",
        Some(DiagnosticSeverity::HINT) => "hint",
        _ => "info",
    };
    let code = code(d).map(|c| format!("[{c}]")).unwrap_or_default();
    let message = d.message.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = format!("{shown}:{}:{column} {severity}{code} {message}", line + 1);
    if !source.trim().is_empty() {
        out.push_str(&format!("\n    {}", source.trim()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{Position, Range};

    /// Against a real rust-analyzer, a type error is reported with its code.
    /// A signature change is reported in the file that calls it.
    /// The fix passes.
    ///
    /// Ignored because it needs rust-analyzer and a toolchain.
    #[tokio::test]
    #[ignore]
    async fn rust_analyzer_reports_what_a_write_broke() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let write = |p: &str, text: &str| {
            let path = root.join(p);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            path
        };
        write(
            "Cargo.toml",
            "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        let lib = write(
            "src/lib.rs",
            "pub fn twice(x: u32) -> u32 {\n    x * 2\n}\n",
        );
        let main = write(
            "src/main.rs",
            "fn main() {\n    println!(\"{}\", probe::twice(2));\n}\n",
        );
        let pool = LspPool::new(root.clone());
        let first = after_write(&pool, &root, std::slice::from_ref(&main)).await;
        assert!(matches!(first, Some(Verdict::Passed { .. })), "{first:?}");

        write(
            "src/lib.rs",
            "pub fn twice(x: u32, y: u32) -> u32 {\n    x * y\n}\n",
        );
        let broke = after_write(&pool, &root, std::slice::from_ref(&lib)).await;
        let Some(Verdict::Failed { report, .. }) = broke else {
            panic!("the caller no longer builds: {broke:?}");
        };
        assert!(report.contains("src/main.rs:2"), "{report}");
        assert!(report.contains("[E0061]"), "{report}");

        write(
            "src/main.rs",
            "fn main() {\n    println!(\"{}\", probe::twice(2, 3));\n}\n",
        );
        let fixed = after_write(&pool, &root, std::slice::from_ref(&main)).await;
        assert!(matches!(fixed, Some(Verdict::Passed { .. })), "{fixed:?}");
        pool.shutdown().await;
    }

    fn diag(severity: DiagnosticSeverity, line: u32, message: &str) -> Diagnostic {
        Diagnostic {
            range: Range::new(Position::new(line, 0), Position::new(line, 1)),
            severity: Some(severity),
            message: message.into(),
            ..Default::default()
        }
    }

    /// A write's result says what its check came to after a blank line, and nowhere else.
    #[test]
    fn a_result_says_what_its_check_came_to() {
        let passed = format!("edited src/a.rs\n\n{CHECK_PASSED} rust-analyzer: no new errors");
        assert_eq!(read(&passed).map(|r| r.0), Some(Outcome::Passed));
        let failed = format!("edited src/a.rs\n\n{CHECK_FAILED}\nrust-analyzer: 1 new error(s)");
        assert_eq!(read(&failed).map(|r| r.0), Some(Outcome::Failed));
        let unchecked = format!("edited src/a.rs\n\n{NOT_CHECKED} rust-analyzer is still indexing");
        assert_eq!(read(&unchecked).map(|r| r.0), Some(Outcome::NotChecked));
        assert_eq!(read("the file says checked by hand"), None);
    }

    /// A diagnostic that moved to another line is the same one, and a second copy of a
    /// message is new.
    #[test]
    fn only_what_a_write_introduced_is_new() {
        let was = vec![
            diag(DiagnosticSeverity::WARNING, 3, "unused variable `y`"),
            diag(DiagnosticSeverity::ERROR, 9, "mismatched types"),
        ];
        let now = vec![
            diag(DiagnosticSeverity::WARNING, 4, "unused variable `y`"),
            diag(DiagnosticSeverity::ERROR, 10, "mismatched types"),
            diag(DiagnosticSeverity::ERROR, 12, "mismatched types"),
        ];
        let (new, kept) = split_new(&was, &now);
        assert_eq!(new.len(), 1);
        assert_eq!(new[0].range.start.line, 12);
        assert_eq!(kept.len(), 2);
    }

    /// Diagnostics at different columns but same line and message are deduplicated.
    #[test]
    fn diagnostics_deduplicated_by_line_and_key() {
        use std::path::Path;

        let base_url = Url::from_file_path(Path::new("/project/test.rs")).unwrap();

        // Two diagnostics with same file, line and message but different columns
        let d1 = Diagnostic {
            range: Range::new(Position::new(5, 0), Position::new(5, 5)),
            severity: Some(DiagnosticSeverity::ERROR),
            message: "type error".into(),
            ..Default::default()
        };
        let d2 = Diagnostic {
            range: Range::new(Position::new(5, 2), Position::new(5, 7)),
            severity: Some(DiagnosticSeverity::ERROR),
            message: "type error".into(),
            ..Default::default()
        };

        let errors: Vec<(Url, &Diagnostic)> =
            vec![(base_url.clone(), &d1), (base_url.clone(), &d2)];
        assert_eq!(first_on_each_line(&errors).len(), 1);
    }
}
