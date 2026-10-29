//! Plugins: commands, agents, skills, hooks and tool servers that travel together.
//!
//! A plugin is a directory in `~/.config/lokode/plugins/`, named by the directory, with a
//! `plugin.toml` and any of `commands/`, `agents/` and `skills/` beside it:
//!
//! ```toml
//! description = "the team's release routine"
//! version = "1.2.0"
//!
//! [hooks]
//! turn-end = "cargo fmt --check"
//!
//! [mcp.servers.tracker]
//! command = "tracker-mcp"
//! ```
//!
//! A plugin is installed by putting its directory there, and turned off by naming it in
//! `[plugins] disabled`. Plugins live in the user's configuration only: a project cannot
//! bring one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// What a plugin's `plugin.toml` says.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct PluginFile {
    #[serde(default)]
    pub description: String,
    pub version: Option<String>,
    #[serde(default)]
    pub hooks: crate::config::HooksSection,
    #[serde(default)]
    pub mcp: crate::config::McpSection,
}

/// One plugin.
#[derive(Debug, Clone)]
pub struct Plugin {
    pub name: String,
    pub dir: PathBuf,
    pub file: PluginFile,
    /// Named in `[plugins] disabled`.
    pub disabled: bool,
}

impl Plugin {
    /// Its directory of `kind`: `commands`, `agents` or `skills`.
    pub fn dir_of(&self, kind: &str) -> PathBuf {
        self.dir.join(kind)
    }
}

/// Where plugins are installed.
pub fn home() -> Option<PathBuf> {
    crate::config::user_config_path().and_then(|p| p.parent().map(|d| d.join("plugins")))
}

/// The plugins installed under `home`, `disabled` naming those turned off. Says what it
/// could not read.
pub fn load_from(home: &Path, disabled: &[String]) -> (Vec<Plugin>, Vec<String>) {
    let mut out = Vec::new();
    let mut warnings = Vec::new();
    let Ok(entries) = std::fs::read_dir(home) else {
        return (out, warnings);
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("plugin.toml").is_file())
        .collect();
    dirs.sort();
    for dir in dirs {
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if !crate::frontmatter::is_name(&name) {
            warnings.push(format!(
                "the plugin {} is not named with letters, digits, - and _ only",
                dir.display()
            ));
            continue;
        }
        let read = std::fs::read_to_string(dir.join("plugin.toml"))
            .map_err(|e| e.to_string())
            .and_then(|t| toml::from_str::<PluginFile>(&t).map_err(|e| e.to_string()));
        match read {
            Ok(file) => out.push(Plugin {
                disabled: disabled.contains(&name),
                name,
                dir,
                file,
            }),
            Err(why) => warnings.push(format!("the plugin {name}: {why}")),
        }
    }
    (out, warnings)
}

/// The plugins installed for this user, with those the user's configuration turns off
/// marked so.
pub fn load() -> (Vec<Plugin>, Vec<String>) {
    let disabled = crate::config::user_config_path()
        .and_then(|p| crate::config::read(&p).ok().flatten())
        .map(|c| c.plugins.disabled)
        .unwrap_or_default();
    match home() {
        Some(home) => load_from(&home, &disabled),
        None => (Vec::new(), Vec::new()),
    }
}

/// The directories of `kind` of the plugins turned on.
pub fn dirs(kind: &str) -> Vec<PathBuf> {
    load()
        .0
        .into_iter()
        .filter(|p| !p.disabled)
        .map(|p| p.dir_of(kind))
        .collect()
}

/// The hooks and the tool servers of the plugins turned on, as one section of each. A
/// server keeps the name its plugin gave it, and the first plugin to use a name keeps it.
pub fn sections(
    plugins: &[Plugin],
) -> (
    crate::config::HooksSection,
    BTreeMap<String, crate::config::McpServer>,
) {
    let mut hooks = crate::config::HooksSection::default();
    let mut servers = BTreeMap::new();
    for p in plugins.iter().filter(|p| !p.disabled) {
        hooks = hooks.after(p.file.hooks.clone());
        for (name, server) in &p.file.mcp.servers {
            servers
                .entry(name.clone())
                .or_insert_with(|| server.clone());
        }
    }
    (hooks, servers)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install(home: &Path, name: &str, toml: &str) -> PathBuf {
        let dir = home.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.toml"), toml).unwrap();
        dir
    }

    /// A plugin is read with what it brings, one turned off is marked, and one that cannot
    /// be read is said.
    #[test]
    fn plugins_are_read_and_turned_off_by_name() {
        let home = tempfile::tempdir().unwrap();
        install(
            home.path(),
            "release",
            "description = \"releases\"\nversion = \"1.0\"\n[hooks]\nturn-end = \"fmt\"\n\
             [mcp.servers.tracker]\ncommand = \"tracker\"\n",
        );
        install(home.path(), "noisy", "[hooks]\nprompt = \"echo hi\"\n");
        install(home.path(), "broken", "colour = \"red\"\n");
        let (plugins, warnings) = load_from(home.path(), &["noisy".to_string()]);
        let names: Vec<_> = plugins
            .iter()
            .map(|p| (p.name.as_str(), p.disabled))
            .collect();
        assert_eq!(names, [("noisy", true), ("release", false)]);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let (hooks, servers) = sections(&plugins);
        assert_eq!(hooks.turn_end.len(), 1);
        assert!(hooks.prompt.is_empty());
        assert!(servers.contains_key("tracker"));
    }
}
