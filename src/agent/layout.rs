//! What the project looks like, put in front of the model before it asks.
//!
//! The shape of the tree goes in with the contract.
//! The model does not have to ask for a file to be pasted in.
//!
//! The map holds names and directories.
//! It is bounded and gitignore-aware.
//! It is enough to name a file in a read call.
//! It does not crowd out the conversation.

use std::path::Path;

/// How much of the window the map may take.
///
/// The map is bounded by what it costs, not by how deep it goes.
/// It must name source files, since a map that stops at directory names cannot name a file.
const MOST_CHARS: usize = 2000;

/// How deep to go before a project is being catalogued rather than mapped.
const DEPTH: usize = 4;

/// Every file the project holds, as its name and its path.
///
/// One walk, written once.
/// The map states what is there and the search reads what is in it.
/// 2 walks with different rules would disagree about what the project contains.
pub fn source_files(root: &Path) -> Vec<(String, std::path::PathBuf)> {
    ignore::WalkBuilder::new(root)
        .max_depth(Some(DEPTH))
        .git_ignore(true)
        // Read the ignore file whether or not the project is a repository.
        // target/ is skipped either way.
        .require_git(false)
        .hidden(true)
        .build()
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter_map(|e| {
            let rel = e.path().strip_prefix(root).ok()?;
            Some((rel.display().to_string(), e.path().to_path_buf()))
        })
        .collect()
}

/// The tree as a list of paths, one per line, directories marked.
///
/// Empty when there is nothing to say.
/// The caller then leaves the section out.
pub fn map(root: &Path) -> String {
    let mut names: Vec<String> = Vec::new();
    let mut used = 0usize;
    for (shown, _) in source_files(root) {
        used += shown.len() + 1;
        if used > MOST_CHARS {
            names.push("... and more".to_string());
            break;
        }
        names.push(shown);
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

    /// The map has to be usable to name a file in a read call.
    /// One that stops at directory names cannot be.
    #[test]
    fn the_map_names_files_deep_enough_to_be_read() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/agent")).unwrap();
        std::fs::write(dir.path().join("src/agent/loop.rs"), "").unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "").unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        let map = map(dir.path());
        assert!(map.contains("src/agent/loop.rs"), "{map}");
        assert!(map.contains("src/main.rs"), "{map}");
        assert!(map.contains("Cargo.toml"), "{map}");
    }

    /// Bounded by what it costs, not by how deep it goes.
    #[test]
    fn a_project_too_big_to_name_says_there_is_more() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        for i in 0..500 {
            std::fs::write(dir.path().join(format!("src/file{i:04}.rs")), "").unwrap();
        }
        let map = map(dir.path());
        assert!(map.len() <= MOST_CHARS + 40, "{} chars", map.len());
        assert!(map.contains("and more"), "{map}");
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
