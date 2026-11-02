//! Sharing with the project the hosts a person agreed to.
//!
//! The hosts agreed to with `Always` are written into `[approve.allow] egress` of the project's
//! `.lokode.toml`.
//! The file is committed, so the next person to work on the project gets the same list.
//! They see it, and trust it, before it acts, as for any other setting that loosens a rule.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// What saving the hosts did.
#[derive(Debug, PartialEq, Eq)]
pub struct Saved {
    pub path: PathBuf,
    /// The hosts that were not in the file and are now.
    pub added: Vec<String>,
    /// The hosts the file already had.
    pub present: Vec<String>,
    /// Whether the file's settings are still trusted for this project.
    ///
    /// False when they were not trusted before the change, since saving a host is not
    /// agreeing to the rest of the file.
    pub trusted: bool,
}

/// The table `key` of `parent`, made when it is not there.
fn child<'a>(parent: &'a mut toml_edit::Table, key: &str) -> Result<&'a mut toml_edit::Table> {
    let item = parent.entry(key).or_insert_with(|| {
        let mut table = toml_edit::Table::new();
        table.set_implicit(true);
        toml_edit::Item::Table(table)
    });
    item.as_table_mut()
        .with_context(|| format!("`{key}` in the project file is not a table"))
}

/// Add `hosts` to the project file at `root`, and keep the file's comments and order.
pub fn save(root: &Path, hosts: &[String]) -> Result<Saved> {
    // What goes into the file is a host name or an address, and never a pattern.
    if let Some(bad) = hosts.iter().find(|h| {
        !crate::sandbox::egress::valid_host(crate::sandbox::approval::split_authority(h).0)
    }) {
        anyhow::bail!("{bad:?} is not a host name, so it is not written");
    }
    let path = crate::config::project_config_path(root);
    let before = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = before
        .parse()
        .with_context(|| format!("{} is not valid TOML", path.display()))?;

    let approve = child(doc.as_table_mut(), "approve")?;
    let allow = child(approve, "allow")?;
    let egress = allow
        .entry("egress")
        .or_insert(toml_edit::Item::Value(toml_edit::Array::new().into()))
        .as_array_mut()
        .context("`egress` in the project file is not a list")?;
    let mut added = Vec::new();
    let mut present = Vec::new();
    for host in hosts {
        let listed = egress.iter().any(|v| v.as_str() == Some(host));
        match listed {
            true => present.push(host.clone()),
            false => {
                egress.push(host.as_str());
                added.push(host.clone());
            }
        }
    }
    if added.is_empty() {
        return Ok(Saved {
            path,
            added,
            present,
            trusted: true,
        });
    }

    let after = doc.to_string();
    let old = crate::trust::split(&before)?;
    let was_trusted = old
        .guarded
        .as_deref()
        .is_none_or(|g| crate::trust::is_trusted(root, g));
    std::fs::write(&path, &after).with_context(|| format!("writing {}", path.display()))?;
    let trusted = was_trusted
        && crate::trust::split(&after)?
            .guarded
            .as_deref()
            .is_none_or(|g| crate::trust::grant(root, g).is_ok());
    Ok(Saved {
        path,
        added,
        present,
        trusted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hosts(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// A pattern is never written to the list of agreed hosts, whatever a command asked for.
    #[test]
    fn a_pattern_is_not_written_as_a_host() {
        let data = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        crate::paths::with_env(&[("XDG_DATA_HOME", data.path().to_str())], || {
            assert!(save(root.path(), &hosts(&["docs.rs", "*"])).is_err());
            assert!(!root.path().join(".lokode.toml").exists());
        });
    }

    /// A host may be written with its port, and a pattern still may not.
    #[test]
    fn a_host_is_written_with_its_port() {
        let data = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        crate::paths::with_env(&[("XDG_DATA_HOME", data.path().to_str())], || {
            let saved = save(root.path(), &hosts(&["docs.rs:443", "[::1]:22"])).unwrap();
            assert_eq!(saved.added, hosts(&["docs.rs:443", "[::1]:22"]));
            assert!(save(root.path(), &hosts(&["*.rs:443"])).is_err());
        });
    }

    /// The hosts join the list that is there, and what the file said stays as it was.
    #[test]
    fn hosts_are_added_to_the_list_and_the_file_keeps_its_comments() {
        let data = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join(".lokode.toml");
        std::fs::write(
            &file,
            "# the build\nmodel = \"big\"\n\n[approve.allow]\n# fetches\negress = [\"docs.rs\"]\n",
        )
        .unwrap();
        crate::paths::with_env(&[("XDG_DATA_HOME", data.path().to_str())], || {
            let saved = save(root.path(), &hosts(&["docs.rs", "crates.io"])).unwrap();
            assert_eq!(saved.added, ["crates.io"]);
            assert_eq!(saved.present, ["docs.rs"]);
        });
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.starts_with("# the build\nmodel = \"big\""), "{text}");
        assert!(text.contains("# fetches"), "{text}");
        assert!(text.contains("\"docs.rs\", \"crates.io\""), "{text}");
    }

    /// A project with no file gets one, with the table the setting lives in.
    #[test]
    fn a_missing_file_is_made_with_the_table() {
        let data = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        crate::paths::with_env(&[("XDG_DATA_HOME", data.path().to_str())], || {
            save(root.path(), &hosts(&["crates.io"])).unwrap();
        });
        let text = std::fs::read_to_string(root.path().join(".lokode.toml")).unwrap();
        let parsed: toml::Table = toml::from_str(&text).unwrap();
        assert_eq!(
            parsed["approve"]["allow"]["egress"].as_array().unwrap()[0].as_str(),
            Some("crates.io")
        );
        assert!(text.contains("[approve.allow]"), "{text}");
        assert!(!text.contains("[approve]"), "{text}");
    }

    /// Saving what is already there changes nothing.
    #[test]
    fn hosts_already_listed_leave_the_file_alone() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join(".lokode.toml");
        let text = "[approve.allow]\negress = [\"docs.rs\"]\n";
        std::fs::write(&file, text).unwrap();
        let saved = save(root.path(), &hosts(&["docs.rs"])).unwrap();
        assert!(saved.added.is_empty());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), text);
    }

    /// Settings the person had trusted stay trusted, so saving a host does not ask again.
    /// Settings they had not trusted stay untrusted: a host is not the whole file.
    #[test]
    fn saving_a_host_keeps_trust_only_where_there_was_some() {
        let data = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join(".lokode.toml");
        crate::paths::with_env(&[("XDG_DATA_HOME", data.path().to_str())], || {
            let first = "[shell]\nsandbox = \"workspace\"\n";
            std::fs::write(&file, first).unwrap();
            let guarded = crate::trust::split(first).unwrap().guarded.unwrap();
            crate::trust::grant(root.path(), &guarded).unwrap();
            assert!(save(root.path(), &hosts(&["a.test"])).unwrap().trusted);
            let now = std::fs::read_to_string(&file).unwrap();
            let guarded = crate::trust::split(&now).unwrap().guarded.unwrap();
            assert!(crate::trust::is_trusted(root.path(), &guarded));

            // Someone else's change arrives, which this person has not read.
            let arrived = format!("{now}\n[hooks]\nturn-end = \"make\"\n");
            std::fs::write(&file, &arrived).unwrap();
            let saved = save(root.path(), &hosts(&["b.test"])).unwrap();
            assert!(!saved.trusted);
            let now = std::fs::read_to_string(&file).unwrap();
            let guarded = crate::trust::split(&now).unwrap().guarded.unwrap();
            assert!(!crate::trust::is_trusted(root.path(), &guarded));
        });
    }

    #[test]
    fn a_file_that_is_not_toml_is_not_overwritten() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join(".lokode.toml");
        std::fs::write(&file, "not [ toml").unwrap();
        assert!(save(root.path(), &hosts(&["a.test"])).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "not [ toml");
    }
}
