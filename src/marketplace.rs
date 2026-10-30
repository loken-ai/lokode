//! Plugin marketplaces: git repositories that list plugins to install by name.
//!
//! A marketplace is cloned into `~/.config/lokode/marketplaces/<name>`.
//! It holds a `marketplace.toml`:
//!
//! ```toml
//! description = "the team's plugins"
//!
//! [[plugins]]
//! name = "release"
//! description = "the release routine"
//! source = "https://example.com/team/lokode-release.git"
//!
//! [[plugins]]
//! name = "fmt"
//! source = "plugins/fmt"
//! ```
//!
//! A source is a git url, or a directory of the marketplace itself.
//! A plugin is installed as `release@team`, or as `release` when one marketplace alone lists it.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

/// The file a marketplace lists its plugins in.
pub const FILE: &str = "marketplace.toml";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MarketFile {
    #[serde(default)]
    description: String,
    #[serde(default)]
    plugins: Vec<Entry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    name: String,
    #[serde(default)]
    description: String,
    version: Option<String>,
    source: String,
}

/// Where a listed plugin comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Git(String),
    /// A directory of a git repository, which is cloned for it.
    Subdir {
        url: String,
        path: String,
    },
    /// A zip archive downloaded over HTTPS.
    Archive(String),
    /// A package of the npm registry, at a version when one is named.
    Npm {
        package: String,
        version: Option<String>,
    },
    /// A directory of the marketplace's own clone.
    Dir(PathBuf),
}

/// A plugin a marketplace lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    pub name: String,
    pub description: String,
    pub version: Option<String>,
    pub source: Source,
    /// The marketplace that lists it.
    pub market: String,
}

/// One marketplace added.
#[derive(Debug, Clone)]
pub struct Market {
    pub name: String,
    pub description: String,
    pub listings: Vec<Listing>,
}

/// Where marketplaces are cloned.
pub fn home() -> Option<PathBuf> {
    crate::config::user_config_path().and_then(|p| p.parent().map(|d| d.join("marketplaces")))
}

/// Whether `source` names a git repository rather than a directory of a marketplace.
fn is_url(source: &str) -> bool {
    source.contains("://") || source.starts_with("git@") || source.ends_with(".git")
}

/// The marketplace cloned in `dir`, named `name`.
fn read(dir: &Path, name: &str) -> Result<Market> {
    if !dir.join(FILE).is_file() && listing_json(dir).is_file() {
        return read_json(dir, name);
    }
    let text = std::fs::read_to_string(dir.join(FILE))
        .map_err(|_| anyhow!("{name} has no {FILE}, so it is not a lokode marketplace"))?;
    let file: MarketFile = toml::from_str(&text).map_err(|e| anyhow!("{name}: {e}"))?;
    let root = dir.canonicalize()?;
    let mut listings = Vec::new();
    for e in file.plugins {
        if !crate::frontmatter::is_name(&e.name) {
            bail!("{name} lists a plugin {:?}, which is not a name", e.name);
        }
        let source = match is_url(&e.source) {
            true => Source::Git(e.source),
            false => {
                // A directory of the marketplace and nowhere else.
                let at = root
                    .join(&e.source)
                    .canonicalize()
                    .with_context(|| format!("{name}/{}: {} is not there", e.name, e.source))?;
                if !at.starts_with(&root) {
                    bail!("{name}/{}: {} leaves the marketplace", e.name, e.source);
                }
                Source::Dir(at)
            }
        };
        listings.push(Listing {
            name: e.name,
            description: e.description,
            version: e.version,
            source,
            market: name.to_string(),
        });
    }
    Ok(Market {
        name: name.to_string(),
        description: file.description,
        listings,
    })
}

/// The file Claude Code's marketplaces list their plugins in.
fn listing_json(dir: &Path) -> PathBuf {
    dir.join(crate::claude::DIR).join("marketplace.json")
}

/// A marketplace written as Claude Code writes them.
fn read_json(dir: &Path, name: &str) -> Result<Market> {
    let text = std::fs::read_to_string(listing_json(dir))?;
    let (description, plugins) =
        crate::claude::marketplace_from(&text).map_err(|e| anyhow!("{name}: {e}"))?;
    let root = dir.canonicalize()?;
    let mut listings = Vec::new();
    for listed in plugins {
        let (plugin, about, version) = (listed.name, listed.description, listed.version);
        if !crate::frontmatter::is_name(&plugin) {
            continue;
        }
        let source = match listed.source {
            Ok(Source::Dir(path)) => {
                // A directory of the marketplace and nowhere else.
                let at = root
                    .join(&path)
                    .canonicalize()
                    .with_context(|| format!("{name}/{plugin}: {} is not there", path.display()))?;
                if !at.starts_with(&root) {
                    bail!("{name}/{plugin}: {} leaves the marketplace", path.display());
                }
                Source::Dir(at)
            }
            Ok(other) => other,
            // A kind of source lokode does not read is left out of what can be installed.
            Err(_) => continue,
        };
        listings.push(Listing {
            name: plugin,
            description: about,
            version,
            source,
            market: name.to_string(),
        });
    }
    Ok(Market {
        name: name.to_string(),
        description,
        listings,
    })
}

/// Every marketplace added under `home`, and what could not be read.
pub fn load(home: &Path) -> (Vec<Market>, Vec<String>) {
    let mut out = Vec::new();
    let mut warnings = Vec::new();
    let Ok(entries) = std::fs::read_dir(home) else {
        return (out, warnings);
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && (p.join(FILE).is_file() || listing_json(p).is_file()))
        .collect();
    dirs.sort();
    for dir in dirs {
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        match read(&dir, &name) {
            Ok(m) => out.push(m),
            Err(e) => warnings.push(format!("{e:#}")),
        }
    }
    (out, warnings)
}

/// Clone the marketplace at `url` into `home`, under `name` or the end of the url.
pub fn add(home: &Path, url: &str, name: Option<&str>) -> Result<Market> {
    let name = match name {
        Some(n) => n.to_string(),
        None => crate::plugins::name_for(url)
            .ok_or_else(|| anyhow!("no name can be read off {url}: give one with --name"))?,
    };
    if !crate::frontmatter::is_name(&name) {
        bail!("a marketplace is named with letters, digits, - and _ only, and {name} is not");
    }
    std::fs::create_dir_all(home)?;
    let dir = home.join(&name);
    if dir.exists() {
        bail!("the marketplace {name} is already added: lokode plugin marketplace update {name}");
    }
    crate::plugins::git(
        home,
        &["clone", "--quiet", "--depth", "1", "--", url, &name],
    )?;
    match read(&dir, &name) {
        Ok(m) => Ok(m),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            Err(e)
        }
    }
}

/// Bring the marketplace `name` up to its source.
pub fn update(home: &Path, name: &str) -> Result<Market> {
    let dir = home.join(name);
    if !dir.join(".git").exists() {
        bail!("no marketplace {name} is added");
    }
    crate::plugins::git(&dir, &["fetch", "--quiet", "--depth", "1", "origin"])?;
    crate::plugins::git(&dir, &["checkout", "--quiet", "--detach", "FETCH_HEAD"])?;
    read(&dir, name)
}

/// Take the marketplace `name` out.
///
/// The plugins installed from it stay.
pub fn remove(home: &Path, name: &str) -> Result<()> {
    if !crate::frontmatter::is_name(name) {
        bail!("no marketplace is called {name}");
    }
    let dir = home.join(name);
    // Only a marketplace is deleted.
    // A directory there without its file is left alone.
    read(&dir, name)?;
    std::fs::remove_dir_all(&dir).with_context(|| dir.display().to_string())
}

/// The listing `spec` names: `plugin@market`.
/// `plugin` alone names it when one marketplace alone lists it.
pub fn find(markets: &[Market], spec: &str) -> Result<Listing> {
    let (plugin, market) = match spec.split_once('@') {
        Some((p, m)) => (p, Some(m)),
        None => (spec, None),
    };
    let found: Vec<&Listing> = markets
        .iter()
        .filter(|m| market.is_none_or(|want| m.name == want))
        .flat_map(|m| &m.listings)
        .filter(|l| l.name == plugin)
        .collect();
    match found.as_slice() {
        [one] => Ok((*one).clone()),
        [] => Err(anyhow!(
            "no marketplace lists {spec}: lokode plugin search shows what they list"
        )),
        many => Err(anyhow!(
            "{plugin} is listed by {}: name one as {plugin}@<marketplace>",
            many.iter()
                .map(|l| l.market.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn market(dir: &Path, toml: &str) {
        std::fs::create_dir_all(dir.join("plugins/fmt")).unwrap();
        std::fs::write(dir.join(FILE), toml).unwrap();
    }

    /// Sources are told apart.
    /// A directory leaving the marketplace is refused.
    /// A plugin is found by name or by name and marketplace.
    #[test]
    fn a_marketplace_lists_its_plugins() {
        let home = tempfile::tempdir().unwrap();
        market(
            &home.path().join("team"),
            "description = \"ours\"\n[[plugins]]\nname = \"release\"\nsource = \"https://x/release.git\"\n\
             [[plugins]]\nname = \"fmt\"\nsource = \"plugins/fmt\"\n",
        );
        market(
            &home.path().join("other"),
            "[[plugins]]\nname = \"fmt\"\nsource = \"git@x:fmt\"\n",
        );
        market(
            &home.path().join("bad"),
            "[[plugins]]\nname = \"out\"\nsource = \"../team\"\n",
        );
        let (markets, warnings) = load(home.path());
        assert_eq!(markets.len(), 2);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let release = find(&markets, "release").unwrap();
        assert_eq!(release.source, Source::Git("https://x/release.git".into()));
        assert!(find(&markets, "fmt")
            .unwrap_err()
            .to_string()
            .contains("other, team"));
        let fmt = find(&markets, "fmt@team").unwrap();
        assert!(matches!(fmt.source, Source::Dir(ref d) if d.ends_with("plugins/fmt")));
        assert!(find(&markets, "none").is_err());
    }
}
