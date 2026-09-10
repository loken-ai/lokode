//! What the project looks like, put in front of the model before it asks.
//!
//! A model used to a chat window asks for the file to be pasted in. It asks because it
//! cannot see anything: the first thing in its context is a request about a project it has
//! been told nothing about, and asking is cheaper than guessing a path. So the shape of the
//! tree goes in with the contract, and the question does not come up.
//!
//! A map, not a copy. Names and directories, bounded, gitignore-aware: enough to name a
//! file in a read call, and not so much that it crowds out the conversation.

use std::path::Path;

/// How many entries the map may name. Past this the model has enough to start reading.
const MOST: usize = 80;

/// How deep to go. The top two levels say what kind of project this is and where its code
/// lives; below that is detail a read call answers better.
const DEPTH: usize = 2;

/// The tree as a list of paths, one per line, directories marked.
///
/// Empty when there is nothing to say, so the caller can leave the section out rather than
/// state that a project has no files.
pub fn map(root: &Path) -> String {
    let walker = ignore::WalkBuilder::new(root)
        .max_depth(Some(DEPTH))
        .git_ignore(true)
        // Read the ignore file whether or not this is a checkout: a project is a project
        // before it is a repository, and target/ is noise either way.
        .require_git(false)
        .hidden(true)
        .build();
    let mut names: Vec<String> = Vec::new();
    for entry in walker.flatten() {
        if entry.depth() == 0 {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let shown = match is_dir {
            true => format!("{}/", rel.display()),
            false => rel.display().to_string(),
        };
        names.push(shown);
        if names.len() >= MOST {
            names.push("…".to_string());
            break;
        }
    }
    names.sort();
    names.join("\n")
}

/// The map as it goes into the system message, or nothing when there is none.
pub fn preamble(root: &Path) -> Option<String> {
    let map = map(root);
    if map.is_empty() {
        return None;
    }
    Some(format!(
        "The project you are working in, as it is on disk. Read what you need from it \
         rather than asking for it:\n{map}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_map_names_directories_and_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "").unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        let map = map(dir.path());
        assert!(map.contains("src/"), "{map}");
        assert!(map.contains("src/main.rs"), "{map}");
        assert!(map.contains("Cargo.toml"), "{map}");
    }

    /// What the project ignores is not part of the project.
    #[test]
    fn what_is_ignored_is_not_named() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "target\n").unwrap();
        std::fs::create_dir(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("target/big"), "").unwrap();
        std::fs::write(dir.path().join("keep.rs"), "").unwrap();
        let map = map(dir.path());
        assert!(map.contains("keep.rs"), "{map}");
        assert!(!map.contains("target/big"), "{map}");
    }

    #[test]
    fn a_project_with_nothing_in_it_says_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(preamble(dir.path()).is_none());
    }
}
