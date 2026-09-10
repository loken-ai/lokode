//! Files named in a prompt, put in front of the model with it.
//!
//! "look at @src/config.rs and tell me why" costs a round trip: the model reads the
//! sentence, calls a tool, waits, and only then starts. The file was named, it is on this
//! disk, and the person naming it has already decided it is relevant - so it travels with
//! the question rather than being fetched after it.
//!
//! On a local model each of those round trips is seconds, which is why this is worth more
//! here than it would be against a hosted one.

use std::path::Path;

/// The text as it will be sent, and what was attached to it.
pub struct Expanded {
    pub text: String,
    /// What was read, for telling the user. Empty when the prompt named nothing.
    pub attached: Vec<String>,
}

/// Attach every file the prompt names with `@`.
///
/// The mention stays where it was written, so the sentence still reads as the person
/// wrote it, and the contents follow at the end where they do not interrupt it.
pub fn expand(text: &str, root: &Path, max_chars: usize) -> Expanded {
    let mut attached = Vec::new();
    let mut bodies = String::new();
    let mut budget = max_chars;

    for token in mentions(text) {
        // Through the sandbox, so a mention cannot reach outside the project any more
        // than a tool call can.
        let Ok(path) = crate::sandbox::allowlist::resolve(root, Path::new(&token), true) else {
            continue;
        };
        let shown = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .display()
            .to_string();
        if attached.contains(&shown) {
            continue;
        }
        let body = if path.is_dir() {
            list_dir(&path)
        } else {
            match std::fs::read_to_string(&path) {
                Ok(t) => t,
                // A file that cannot be read as text is not attached and not complained
                // about: the agent still has a tool for it.
                Err(_) => continue,
            }
        };
        if budget == 0 {
            break;
        }
        let (body, cut) = if body.len() > budget {
            let end = (0..=budget)
                .rev()
                .find(|i| body.is_char_boundary(*i))
                .unwrap_or(0);
            (&body[..end], true)
        } else {
            (body.as_str(), false)
        };
        budget = budget.saturating_sub(body.len());
        bodies.push_str(&format!("\n\n--- {shown} ---\n{body}"));
        if cut {
            bodies.push_str("\n… (the rest was left out; read the file for it)");
        }
        attached.push(shown);
    }

    let text = if bodies.is_empty() {
        text.to_string()
    } else {
        format!("{text}\n{bodies}")
    };
    Expanded { text, attached }
}

/// The paths a prompt mentions.
///
/// A mention is `@` against something that could be a path. Trailing punctuation is left
/// out, because "@src/main.rs," is a sentence, and an address like a@b is not a mention:
/// the `@` has to start the word.
fn mentions(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter_map(|word| word.strip_prefix('@'))
        .map(|w| w.trim_end_matches([',', '.', ';', ':', ')', ']', '?', '!']))
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

fn list_dir(path: &Path) -> String {
    let mut names: Vec<String> = std::fs::read_dir(path)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    match e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        true => format!("{name}/"),
                        false => name,
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.path().join("notes.md"), "some notes\n").unwrap();
        dir
    }

    #[test]
    fn a_named_file_travels_with_the_question() {
        let dir = project();
        let out = expand("why does @src/main.rs do nothing?", dir.path(), 10_000);
        assert_eq!(out.attached, vec!["src/main.rs"]);
        // The sentence is left as it was written.
        assert!(out.text.starts_with("why does @src/main.rs do nothing?"));
        assert!(out.text.contains("--- src/main.rs ---"));
        assert!(out.text.contains("fn main() {}"));
    }

    #[test]
    fn a_directory_comes_as_its_listing() {
        let dir = project();
        let out = expand("what is in @src", dir.path(), 10_000);
        assert_eq!(out.attached, vec!["src"]);
        assert!(out.text.contains("main.rs"));
    }

    /// Nothing that is not a path is attached, and nothing outside the project is
    /// reachable by naming it.
    #[test]
    fn what_is_not_a_file_here_is_left_alone() {
        let dir = project();
        let out = expand(
            "mail someone@example.com about @nowhere.rs and @../../etc/passwd",
            dir.path(),
            10_000,
        );
        assert!(out.attached.is_empty());
        assert_eq!(
            out.text,
            "mail someone@example.com about @nowhere.rs and @../../etc/passwd"
        );
    }

    #[test]
    fn the_same_file_named_twice_is_attached_once() {
        let dir = project();
        let out = expand("@notes.md and @notes.md again", dir.path(), 10_000);
        assert_eq!(out.attached.len(), 1);
    }

    /// What is attached is bounded like anything else that lands in the window, and the
    /// cut is said so the model does not read a file that stops mid-line as the whole of
    /// it.
    #[test]
    fn what_does_not_fit_is_cut_and_said() {
        let dir = project();
        std::fs::write(dir.path().join("big.txt"), "x".repeat(5000)).unwrap();
        let out = expand("read @big.txt", dir.path(), 200);
        assert!(out.text.len() < 600);
        assert!(out.text.contains("the rest was left out"));
    }
}
