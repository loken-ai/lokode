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
//! A plugin is installed with `lokode plugin install <git url>`, or by putting its directory there.
//! It is turned off by naming it in `[plugins] disabled`.
//! Plugins live in the user's configuration only: a project cannot bring one.
//!
//! What a plugin runs is shown before it is installed, and again when an update changes it.
//! Its hooks and its servers run with the user's rights.

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
    #[serde(default)]
    pub lsp: crate::config::LspSection,
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
            ("session-end", &h.session_end),
            ("notification", &h.notification),
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

/// The plugins installed under `home`, `disabled` naming those turned off.
/// Says what it could not read.
pub fn load_from(home: &Path, disabled: &[String]) -> (Vec<Plugin>, Vec<String>) {
    let mut out = Vec::new();
    let mut warnings = Vec::new();
    let Ok(entries) = std::fs::read_dir(home) else {
        return (out, warnings);
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("plugin.toml").is_file() || crate::claude::is_plugin(p))
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
        match file_of(&dir) {
            Ok((file, notes)) => {
                warnings.extend(notes.into_iter().map(|n| format!("the plugin {name}: {n}")));
                out.push(Plugin {
                    disabled: disabled.contains(&name),
                    name,
                    dir,
                    file,
                })
            }
            Err(why) => warnings.push(format!("the plugin {name}: {why}")),
        }
    }
    (out, warnings)
}

/// The plugins installed for this user, with those the user's configuration turns off marked so.
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

/// What the plugins turned on add to the configuration.
#[derive(Default)]
pub struct Sections {
    pub hooks: crate::config::HooksSection,
    pub servers: BTreeMap<String, crate::config::McpServer>,
    pub lsp: BTreeMap<String, crate::config::LspServer>,
}

/// The hooks, the tool servers and the language servers of the plugins turned on.
/// A server keeps the name its plugin gave it, and the first plugin to use a name keeps it.
pub fn sections(plugins: &[Plugin]) -> Sections {
    let mut out = Sections::default();
    for p in plugins.iter().filter(|p| !p.disabled) {
        out.hooks = out.hooks.after(p.file.hooks.clone());
        for (name, server) in &p.file.mcp.servers {
            out.servers
                .entry(name.clone())
                .or_insert_with(|| server.clone());
        }
        for (name, server) in &p.file.lsp.servers {
            out.lsp
                .entry(name.clone())
                .or_insert_with(|| server.clone());
        }
    }
    out
}

/// Where a plugin is fetched before it is installed.
/// It is inside the plugins' directory, so it is moved into place without a copy.
/// It is one level down, so it is never loaded.
const FETCHING: &str = ".fetching";

/// The name a plugin from `source` is installed under, when none is given.
/// It is the last part of the source, without `.git`.
pub fn name_for(source: &str) -> Option<String> {
    let last = source
        .trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()?
        .trim_end_matches(".git");
    crate::frontmatter::is_name(last).then(|| last.to_string())
}

/// Run git in `dir` with `args`, and say what it printed when it fails.
pub(crate) fn git(dir: &Path, args: &[&str]) -> Result<String> {
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

/// What the plugin in `dir` brings, from its `plugin.toml` or, when it has none, from the layout
/// Claude Code's plugins have.
/// Says what it leaves out of the second.
fn file_of(dir: &Path) -> std::result::Result<(PluginFile, Vec<String>), String> {
    if dir.join("plugin.toml").is_file() {
        let text = std::fs::read_to_string(dir.join("plugin.toml")).map_err(|e| e.to_string())?;
        let file = toml::from_str::<PluginFile>(&text).map_err(|e| e.to_string())?;
        return Ok((file, Vec::new()));
    }
    if !crate::claude::is_plugin(dir) {
        return Err("it has no plugin.toml and holds nothing lokode reads".into());
    }
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let given = crate::config::user_config_path()
        .and_then(|p| crate::config::read(&p).ok().flatten())
        .and_then(|c| c.plugins.options.get(&name).cloned())
        .unwrap_or_default();
    let read = crate::claude::read_plugin(dir, &given)?;
    Ok((
        PluginFile {
            description: read.description,
            version: read.version,
            hooks: read.hooks,
            mcp: crate::config::McpSection {
                servers: read.servers,
            },
            lsp: crate::config::LspSection {
                enabled: None,
                servers: read.lsp,
            },
        },
        read.warnings,
    ))
}

/// Read the plugin in `dir`, named `name`.
fn read_plugin(dir: &Path, name: &str) -> Result<Plugin> {
    let (file, _) = file_of(dir).map_err(|e| anyhow!("{name}: {e}"))?;
    Ok(Plugin {
        name: name.to_string(),
        dir: dir.to_path_buf(),
        file,
        disabled: false,
    })
}

/// Install the plugin at the git `source` under `home`, named `name`.
/// `agree` is shown the plugin before it goes in place, and says whether it does.
pub fn install(
    home: &Path,
    source: &str,
    name: &str,
    agree: impl FnOnce(&Plugin) -> bool,
) -> Result<Option<Plugin>> {
    land(home, name, agree, |fetching| {
        git(
            fetching,
            &["clone", "--quiet", "--depth", "1", "--", source, name],
        )
        .map(|_| ())
    })
}

/// The file a plugin copied from a marketplace keeps its marketplace in.
pub const FROM_MARKET: &str = ".marketplace";

/// Install the plugin in the marketplace directory `dir` under `home`, named `name`, as `install`
/// does from git.
/// It keeps the name of `market`, to be updated from it.
pub fn install_dir(
    home: &Path,
    dir: &Path,
    market: &str,
    name: &str,
    agree: impl FnOnce(&Plugin) -> bool,
) -> Result<Option<Plugin>> {
    land(home, name, agree, |fetching| {
        let landing = fetching.join(name);
        copy_tree(dir, &landing)?;
        std::fs::write(landing.join(FROM_MARKET), market)?;
        Ok(())
    })
}

/// Fetch a plugin into the fetching directory with `fetch`, show it to `agree`, and put it in place
/// under `home` as `name` when agreed to.
fn land(
    home: &Path,
    name: &str,
    agree: impl FnOnce(&Plugin) -> bool,
    fetch: impl FnOnce(&Path) -> Result<()>,
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
    let fetched = fetch(&fetching).and_then(|_| read_plugin(&landing, name));
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

/// Copy the tree at `from` to `to`, leaving out git's directory and every link.
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let target = to.join(entry.file_name());
        if entry.file_name() == ".git" || kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// The marketplace the installed plugin `name` was copied from, when it was.
pub fn market_of(home: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(home.join(name).join(FROM_MARKET))
        .ok()
        .map(|m| m.trim().to_string())
}

/// Put the plugin `source` names in `landing`, a directory that is not there yet.
pub fn fetch_into(source: &crate::marketplace::Source, landing: &Path) -> Result<()> {
    use crate::marketplace::Source;
    let beside = landing.parent().unwrap_or(landing);
    std::fs::create_dir_all(beside)?;
    let leaf = landing
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let scratch_name = format!("{leaf}-source");
    let scratch = beside.join(&scratch_name);
    let _ = std::fs::remove_dir_all(&scratch);
    let done = (|| -> Result<()> {
        match source {
            Source::Dir(dir) => copy_tree(dir, landing),
            Source::Git(url) => {
                git(
                    beside,
                    &["clone", "--quiet", "--depth", "1", "--", url, &leaf],
                )?;
                Ok(())
            }
            Source::Subdir { url, path } => {
                if path.split('/').any(|p| p == "..") || path.starts_with('/') {
                    bail!("{path} leaves the repository");
                }
                git(
                    beside,
                    &["clone", "--quiet", "--depth", "1", "--", url, &scratch_name],
                )?;
                let real = scratch.join(path).canonicalize()?;
                if !real.starts_with(scratch.canonicalize()?) {
                    bail!("{path} leaves the repository");
                }
                copy_tree(&real, landing)
            }
            Source::Archive(url) => {
                let bytes = download(url)?;
                unpack(&bytes, &scratch)?;
                copy_tree(&single_root(&scratch), landing)
            }
            Source::Npm { package, version } => {
                std::fs::create_dir_all(&scratch)?;
                let spec = match version {
                    Some(v) => format!("{package}@{v}"),
                    None => package.clone(),
                };
                let packed = std::process::Command::new("npm")
                    .args(["pack", "--silent", "--pack-destination"])
                    .arg(&scratch)
                    .arg(&spec)
                    .current_dir(&scratch)
                    .output()
                    .context("running npm, which an npm plugin needs")?;
                if !packed.status.success() {
                    bail!(
                        "npm pack {spec}: {}",
                        String::from_utf8_lossy(&packed.stderr).trim()
                    );
                }
                let file = String::from_utf8_lossy(&packed.stdout)
                    .lines()
                    .last()
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                let out = scratch.join("unpacked");
                std::fs::create_dir_all(&out)?;
                let extracted = std::process::Command::new("tar")
                    .arg("-xzf")
                    .arg(scratch.join(&file))
                    .arg("-C")
                    .arg(&out)
                    .output()?;
                if !extracted.status.success() {
                    bail!(
                        "tar {file}: {}",
                        String::from_utf8_lossy(&extracted.stderr).trim()
                    );
                }
                copy_tree(&single_root(&out), landing)
            }
        }
    })();
    let _ = std::fs::remove_dir_all(&scratch);
    done
}

/// The bytes at `url`, over HTTPS.
fn download(url: &str) -> Result<Vec<u8>> {
    // Over https, or over http to this machine.
    let local = url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h == "localhost" || h == "127.0.0.1"))
        .unwrap_or(false);
    if !url.starts_with("https://") && !(local && url.starts_with("http://")) {
        bail!("{url} is not an https address");
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let resp = reqwest::get(url).await.with_context(|| url.to_string())?;
        if !resp.status().is_success() {
            bail!("{url} answered {}", resp.status());
        }
        Ok(resp.bytes().await?.to_vec())
    })
}

/// Unpack a zip archive into `to`, refusing any name that would leave it.
fn unpack(bytes: &[u8], to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    use std::io::Read;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
    for at in 0..archive.len() {
        let mut entry = archive.by_index(at)?;
        // Ordinary files and directories only, named inside `to`.
        // A link or a name that leaves it is not a part of a plugin.
        let Some(name) = entry.enclosed_name() else {
            continue;
        };
        if entry.is_symlink() {
            continue;
        }
        let target = to.join(name);
        if entry.is_dir() {
            std::fs::create_dir_all(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes)?;
        std::fs::write(&target, bytes)?;
        // Readable, and runnable when the archive says so.
        // The special bits the archive may carry are never kept.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let runnable = entry.unix_mode().is_some_and(|m| m & 0o111 != 0);
            std::fs::set_permissions(
                &target,
                std::fs::Permissions::from_mode(if runnable { 0o755 } else { 0o644 }),
            )?;
        }
    }
    Ok(())
}

/// The directory a tree is really kept in.
/// It is the one entry beneath `dir` when that is all there is, as in an archive of a repository.
/// Otherwise it is `dir` itself.
fn single_root(dir: &Path) -> PathBuf {
    let entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect();
    match entries.as_slice() {
        [one] if one.is_dir() => one.clone(),
        _ => dir.to_path_buf(),
    }
}

/// Install the plugin `source` names under `home`, named `name`.
/// It keeps the name of `market`, to be updated from it.
pub fn install_source(
    home: &Path,
    source: &crate::marketplace::Source,
    market: &str,
    name: &str,
    agree: impl FnOnce(&Plugin) -> bool,
) -> Result<Option<Plugin>> {
    land(home, name, agree, |fetching| {
        let landing = fetching.join(name);
        fetch_into(source, &landing)?;
        std::fs::write(landing.join(FROM_MARKET), market)?;
        Ok(())
    })
}

/// Bring the plugin `name` under `home` to what `source` holds now, asking `agree` as `update`
/// does.
/// Says what changed.
pub fn update_source(
    home: &Path,
    source: &crate::marketplace::Source,
    name: &str,
    agree: impl FnOnce(&Plugin) -> bool,
) -> Result<String> {
    update_with(home, name, |landing| fetch_into(source, landing), agree)
}

/// Bring the plugin `name` under `home` to what the marketplace directory `dir` holds now, asking
/// `agree` as `update` does.
/// Says what changed.
pub fn update_from_dir(
    home: &Path,
    dir: &Path,
    name: &str,
    agree: impl FnOnce(&Plugin) -> bool,
) -> Result<String> {
    update_with(home, name, |landing| copy_tree(dir, landing), agree)
}

fn update_with(
    home: &Path,
    name: &str,
    fetch: impl FnOnce(&Path) -> Result<()>,
    agree: impl FnOnce(&Plugin) -> bool,
) -> Result<String> {
    let installed = home.join(name);
    let before = read_plugin(&installed, name)?;
    let market = market_of(home, name).unwrap_or_default();
    let fetching = home.join(FETCHING);
    let landing = fetching.join(name);
    if landing.exists() {
        std::fs::remove_dir_all(&landing)?;
    }
    if let Err(e) = fetch(&landing) {
        let _ = std::fs::remove_dir_all(&landing);
        let _ = std::fs::remove_dir(&fetching);
        return Err(e);
    }
    std::fs::write(landing.join(FROM_MARKET), &market)?;
    let after = read_plugin(&landing, name)?;
    if same_tree(&installed, &landing) {
        std::fs::remove_dir_all(&landing)?;
        let _ = std::fs::remove_dir(&fetching);
        return Ok(format!("{name} is up to date"));
    }
    if after.file.runs() != before.file.runs() && !agree(&after) {
        std::fs::remove_dir_all(&landing)?;
        let _ = std::fs::remove_dir(&fetching);
        return Ok(format!("{name} left as it was"));
    }
    std::fs::remove_dir_all(&installed)?;
    std::fs::rename(&landing, &installed)?;
    let _ = std::fs::remove_dir(&fetching);
    Ok(format!("{name} updated from {market}"))
}

/// Whether the trees at `a` and `b` hold the same files with the same contents.
fn same_tree(a: &Path, b: &Path) -> bool {
    fn files(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if let Ok(bytes) = std::fs::read(&path) {
                    out.push((
                        path.strip_prefix(root).unwrap_or(&path).to_path_buf(),
                        bytes,
                    ));
                }
            }
        }
        out.sort();
        out
    }
    files(a) == files(b)
}

/// Bring the installed plugin `name` under `home` up to its source.
/// `agree` is shown the plugin as it would be when it runs something new, and says whether to take
/// it.
/// Says what changed.
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
    // What the update would be, unpacked beside the plugin to read what it runs.
    let unpacked = home.join(FETCHING).join(format!("{name}-update"));
    let _ = std::fs::remove_dir_all(&unpacked);
    std::fs::create_dir_all(&unpacked)?;
    let archived = std::process::Command::new("sh")
        .arg("-c")
        .arg("git archive FETCH_HEAD | tar -x -C \"$1\"")
        .arg("sh")
        .arg(&unpacked)
        .current_dir(&dir)
        .output()?;
    let read = match archived.status.success() {
        true => file_of(&unpacked).map_err(|e| anyhow!("{name}: {e}")),
        false => Err(anyhow!(
            "{name}: could not read what the update holds: {}",
            String::from_utf8_lossy(&archived.stderr).trim()
        )),
    };
    let _ = std::fs::remove_dir_all(&unpacked);
    let _ = std::fs::remove_dir(home.join(FETCHING));
    let after = Plugin {
        file: read?.0,
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

    /// A plugin is read with what it brings.
    /// One turned off is marked.
    /// One that cannot be read is said.
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
        let added = sections(&plugins);
        assert_eq!(added.hooks.turn_end.len(), 1);
        assert!(added.hooks.prompt.is_empty());
        assert!(added.servers.contains_key("tracker"));
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

    /// An archive is unpacked as ordinary files.
    /// A link and a name that leaves the directory are left out.
    /// The special permission bits are not kept.
    #[test]
    fn an_archive_is_unpacked_safely() {
        use std::io::Write;
        let mut bytes = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut bytes));
            let plain = zip::write::SimpleFileOptions::default();
            zip.start_file("plug/plugin.toml", plain).unwrap();
            zip.write_all(b"description = \"z\"\n").unwrap();
            zip.start_file("plug/run.sh", plain.unix_permissions(0o4755))
                .unwrap();
            zip.write_all(b"#!/bin/sh\n").unwrap();
            zip.start_file("../escape.txt", plain).unwrap();
            zip.write_all(b"out").unwrap();
            zip.add_symlink("plug/link", "/etc/passwd", plain).unwrap();
            zip.finish().unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let to = dir.path().join("out");
        unpack(&bytes, &to).unwrap();
        assert!(to.join("plug/plugin.toml").is_file());
        assert!(!dir.path().join("escape.txt").exists());
        assert!(!to.join("plug/link").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(to.join("plug/run.sh"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o7777, 0o755, "{mode:o}");
        }
        assert_eq!(single_root(&to), to.join("plug"));
    }

    /// A plugin is installed from git when agreed to, not otherwise.
    /// It is updated when its source moves.
    /// It is asked about again when what it runs changes.
    /// It can be removed.
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

    /// A plugin copied from a marketplace remembers it.
    /// An update copies it again, asking when what it runs changes.
    #[test]
    fn a_plugin_is_copied_from_a_marketplace() {
        let dir = tempfile::tempdir().unwrap();
        let listed = dir.path().join("market/plugins/notes");
        let home = dir.path().join("plugins");
        std::fs::create_dir_all(listed.join("commands")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(listed.join("plugin.toml"), "description = \"one\"\n").unwrap();
        std::fs::write(listed.join("commands/note.md"), "write a note").unwrap();
        let p = install_dir(&home, &listed, "team", "notes", |_| true)
            .unwrap()
            .unwrap();
        assert_eq!(p.brings(), ["1 command"]);
        assert_eq!(market_of(&home, "notes").as_deref(), Some("team"));
        assert!(update_from_dir(&home, &listed, "notes", |_| true)
            .unwrap()
            .contains("up to date"));
        std::fs::write(
            listed.join("plugin.toml"),
            "description = \"two\"\n[hooks]\nprompt = \"echo\"\n",
        )
        .unwrap();
        assert!(update_from_dir(&home, &listed, "notes", |_| false)
            .unwrap()
            .contains("left as it was"));
        assert!(update_from_dir(&home, &listed, "notes", |_| true)
            .unwrap()
            .contains("updated from team"));
        let (loaded, _) = load_from(&home, &[]);
        assert_eq!(loaded[0].file.description, "two");
    }
}
