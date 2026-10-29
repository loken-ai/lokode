//! Whether a project's own configuration may act on this machine.
//!
//! Some settings of a project's `.lokode.toml` run programs, loosen what goes ahead without
//! a question, widen what the shell may write, send requests elsewhere or write files
//! outside the project. Those apply once the user has trusted them for this project, and
//! again after each change to them. The rest of the file applies at once.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The top-level tables of a project file that apply only once trusted.
pub const GUARDED: &[&str] = &["hooks", "mcp", "lsp", "shell", "backend", "web", "session"];

/// A project file, split into what applies at once and what waits for trust.
#[derive(Debug, Clone, PartialEq)]
pub struct Split {
    /// Everything but the guarded settings.
    pub open: toml::Table,
    /// The guarded settings as TOML, or `None` when the file has none.
    pub guarded: Option<String>,
}

/// Split the text of a project file.
pub fn split(text: &str) -> Result<Split> {
    let mut open: toml::Table = toml::from_str(text)?;
    let mut guarded = toml::Table::new();
    for key in GUARDED {
        if let Some(value) = open.remove(*key) {
            guarded.insert(key.to_string(), value);
        }
    }
    if let Some(toml::Value::Table(approve)) = open.get_mut("approve") {
        if let Some(allow) = approve.remove("allow") {
            let mut table = toml::Table::new();
            table.insert("allow".into(), allow);
            guarded.insert("approve".into(), toml::Value::Table(table));
        }
    }
    let guarded = (!guarded.is_empty())
        .then(|| toml::to_string(&guarded))
        .transpose()?;
    Ok(Split { open, guarded })
}

/// Where the trusted settings are kept, by project.
fn store_path() -> Option<PathBuf> {
    crate::paths::data_dir().map(|d| d.join("trusted.json"))
}

fn read_store(path: &Path) -> BTreeMap<String, String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Whether `guarded` is what the user trusted for the project at `root`.
pub fn is_trusted(root: &Path, guarded: &str) -> bool {
    let Some(path) = store_path() else {
        return false;
    };
    read_store(&path)
        .get(&root.display().to_string())
        .map(String::as_str)
        == Some(guarded)
}

/// Record that the user trusts `guarded` for the project at `root`.
pub fn grant(root: &Path, guarded: &str) -> Result<()> {
    let path = store_path().context("no home directory to keep the trusted projects in")?;
    let mut store = read_store(&path);
    store.insert(root.display().to_string(), guarded.to_string());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(&store)?)
        .with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What runs programs or loosens approvals waits, the rest applies, and a refusal
    /// stays with what applies.
    #[test]
    fn the_guarded_settings_are_set_apart() {
        let s = split(
            r#"
            model = "qwen3:8b"
            [hooks]
            turn-end = "cargo fmt"
            [approve.allow]
            bash = ["*"]
            [approve.deny]
            bash = ["git push*"]
            [mcp.servers.x]
            command = "x"
            "#,
        )
        .unwrap();
        assert!(s.open.contains_key("model"));
        assert!(!s.open.contains_key("hooks") && !s.open.contains_key("mcp"));
        let approve = s.open["approve"].as_table().unwrap();
        assert!(approve.contains_key("deny") && !approve.contains_key("allow"));
        let guarded = s.guarded.unwrap();
        assert!(guarded.contains("cargo fmt") && guarded.contains("[approve.allow]"));
        assert!(!guarded.contains("git push"));
    }

    /// A file with nothing guarded needs no trust.
    #[test]
    fn a_harmless_file_needs_nothing() {
        let s = split("model = \"m\"\n[approve.deny]\nbash = [\"x\"]\n").unwrap();
        assert_eq!(s.guarded, None);
    }

    /// Trust holds for the settings trusted, and a change to them asks again.
    #[test]
    fn trust_is_for_what_was_shown() {
        let data = tempfile::tempdir().unwrap();
        let home = data.path().to_str().unwrap();
        crate::paths::with_env(&[("XDG_DATA_HOME", Some(home))], || {
            let root = Path::new("/somewhere/project");
            assert!(!is_trusted(root, "[hooks]\n"));
            grant(root, "[hooks]\n").unwrap();
            assert!(is_trusted(root, "[hooks]\n"));
            assert!(!is_trusted(root, "[hooks]\nturn-end = \"x\"\n"));
            assert!(!is_trusted(Path::new("/elsewhere"), "[hooks]\n"));
        });
    }
}
