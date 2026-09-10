use std::path::{Path, PathBuf};

use thiserror::Error;

/// A refusal, with where the file the caller meant actually is.
///
/// The suggestion is built by the caller rather than by the resolver: finding it walks the
/// project, which is not something a path check should ever do on its own.
pub fn refusal(root: &Path, wanted: &Path, error: &PathError) -> String {
    // Named the way the project names it. The absolute path says where this checkout
    // happens to sit, which is not what the reader is looking for and not how they typed
    // it.
    let mut out = match error {
        PathError::NotFound { path } => format!(
            "path '{}' does not exist",
            path.strip_prefix(root).unwrap_or(path).display()
        ),
        other => other.to_string(),
    };
    if matches!(error, PathError::NotFound { .. }) {
        let near = nearby(root, wanted);
        if !near.is_empty() {
            out.push_str(&format!("; there is one at {}", near.join(", ")));
        }
    }
    out
}

#[derive(Debug, Error)]
pub enum PathError {
    #[error("path '{path}' does not exist")]
    NotFound { path: PathBuf },
    #[error("path '{path}' is outside the project root ({root})")]
    Outside { path: PathBuf, root: PathBuf },
    #[error("io error resolving '{path}': {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Resolves `candidate` (which may be relative) against `root`, then
/// canonicalises it and verifies the result is a descendant of `root`.
///
/// `must_exist = false` allows resolving a target that has not been created yet
/// (write_file, apply_patch). In that case the deepest ancestor that does exist is
/// canonicalised and the names below it are appended, so a file can be placed in
/// directories that do not exist either. The containment check is applied to the result
/// eitherway, and is what makes that safe.
pub fn resolve(root: &Path, candidate: &Path, must_exist: bool) -> Result<PathBuf, PathError> {
    let root_canon = root.canonicalize().map_err(|source| PathError::Io {
        path: root.to_path_buf(),
        source,
    })?;

    let absolute = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root_canon.join(candidate)
    };

    let resolved = if must_exist || absolute.exists() {
        absolute.canonicalize().map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                PathError::NotFound {
                    path: absolute.clone(),
                }
            } else {
                PathError::Io {
                    path: absolute.clone(),
                    source,
                }
            }
        })?
    } else {
        // A name that does not resolve may still exist as a symlink whose target has not
        // been created yet: `exists()` follows the link and reports false, so the parent's
        // canonical form says "inside the root" while a write through that name lands
        // wherever the link points. Follow the link itself before trusting the name.
        let via_link = match absolute.symlink_metadata() {
            Ok(m) if m.file_type().is_symlink() => Some(std::fs::read_link(&absolute).map_err(
                |source| PathError::Io {
                    path: absolute.clone(),
                    source,
                },
            )?),
            _ => None,
        };
        let absolute = match via_link {
            Some(t) if t.is_absolute() => t,
            Some(t) => absolute.parent().unwrap_or(&root_canon).join(t),
            None => absolute,
        };
        // Everything below the deepest ancestor that exists is a name nothing has been
        // created under yet. Requiring the immediate parent to exist meant a write to
        // `src/new/thing.rs` was refused before the code that creates directories ever
        // ran, which is a thing an agent tries constantly.
        //
        // Safe to build a path through names that do not exist: a component that is not
        // there cannot be a symlink, so none of them can redirect the result. The part
        // that does exist is canonicalised, which resolves any link within it, and the
        // containment check below still has the last word - a `..` among the new names
        // simply lands the result outside the root and is refused there.
        let mut new_names: Vec<std::ffi::OsString> = Vec::new();
        let mut cursor = absolute.as_path();
        let existing = loop {
            if cursor.exists() {
                break cursor;
            }
            match (cursor.file_name(), cursor.parent()) {
                (Some(name), Some(parent)) => {
                    new_names.push(name.to_os_string());
                    cursor = parent;
                }
                // Walked off the top without finding anything that exists.
                _ => {
                    return Err(PathError::NotFound {
                        path: absolute.clone(),
                    })
                }
            }
        };
        let mut resolved = existing.canonicalize().map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                PathError::NotFound {
                    path: existing.to_path_buf(),
                }
            } else {
                PathError::Io {
                    path: existing.to_path_buf(),
                    source,
                }
            }
        })?;
        for name in new_names.iter().rev() {
            resolved.push(name);
        }
        resolved
    };

    if !resolved.starts_with(&root_canon) {
        return Err(PathError::Outside {
            path: resolved,
            root: root_canon,
        });
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    /// The absolute path says where this checkout happens to sit, which is not what the
    /// reader is looking for and not how they typed it.
    #[test]
    fn a_refusal_names_the_path_the_way_the_project_does() {
        let root = PathBuf::from("/somewhere/project");
        let missing = root.join("src/gone.rs");
        let said = refusal(
            &root,
            std::path::Path::new("src/gone.rs"),
            &PathError::NotFound {
                path: missing.clone(),
            },
        );
        assert!(said.contains("src/gone.rs"), "{said}");
        assert!(!said.contains("/somewhere/project"), "{said}");
    }

    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn resolves_inside_root() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("a.txt");
        fs::write(&file, b"x").unwrap();
        let r = resolve(dir.path(), Path::new("a.txt"), true).unwrap();
        assert_eq!(r, file.canonicalize().unwrap());
    }

    #[test]
    fn rejects_outside_absolute() {
        let dir = tempdir().unwrap();
        let err = resolve(dir.path(), Path::new("/etc/passwd"), true).unwrap_err();
        assert!(matches!(err, PathError::Outside { .. }));
    }

    #[test]
    fn rejects_parent_escape() {
        let dir = tempdir().unwrap();
        let err = resolve(dir.path(), Path::new("../etc/passwd"), false).unwrap_err();
        // Either Outside (if the parent escape canonicalises) or NotFound — both acceptable rejections.
        assert!(matches!(
            err,
            PathError::Outside { .. } | PathError::NotFound { .. }
        ));
    }

    #[test]
    fn rejects_symlink_escape() {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let secret = outside.path().join("secret.txt");
        fs::write(&secret, b"sshh").unwrap();
        let link = dir.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&secret, &link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&secret, &link).unwrap();

        let err = resolve(dir.path(), Path::new("link"), true).unwrap_err();
        assert!(matches!(err, PathError::Outside { .. }));
    }

    #[test]
    fn allows_new_file_with_existing_parent() {
        let dir = tempdir().unwrap();
        let r = resolve(dir.path(), Path::new("new.txt"), false).unwrap();
        assert!(r.starts_with(dir.path().canonicalize().unwrap()));
        assert_eq!(r.file_name().unwrap(), "new.txt");
    }
}

#[cfg(test)]
mod escape_tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    /// A symlink pointing at a file that does not exist yet is not caught by
    /// `exists()`, so resolution falls to the branch that canonicalises only the
    /// parent - and a write through that name lands wherever the link points.
    #[test]
    fn rejects_a_dangling_symlink_that_leaves_the_root() {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let target = outside.path().join("not-created-yet.txt");
        let link = dir.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&target, &link).unwrap();
        assert!(
            !link.exists(),
            "the link must dangle for this to be the case under test"
        );

        let resolved = resolve(dir.path(), Path::new("link"), false);
        if let Ok(p) = &resolved {
            // Demonstrate the consequence rather than asserting on the path alone.
            fs::write(p, b"escaped").unwrap();
            assert!(
                !target.exists(),
                "writing through the resolved path escaped the root: {} landed outside",
                p.display()
            );
        }
        assert!(
            matches!(resolved, Err(PathError::Outside { .. })),
            "a dangling link out of the root must be refused, got {resolved:?}"
        );
    }
}

#[cfg(test)]
mod new_directory_tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    /// A file in a directory that does not exist yet is the ordinary way an agent adds a
    /// module. Requiring the immediate parent to exist refused it outright.
    #[test]
    fn a_file_under_new_directories_resolves() {
        let dir = tempdir().unwrap();
        let r = resolve(dir.path(), Path::new("src/deep/new.rs"), false).unwrap();
        assert!(r.starts_with(dir.path().canonicalize().unwrap()));
        assert!(r.ends_with("src/deep/new.rs"), "got {}", r.display());
    }

    /// The containment check has the last word, so new names cannot be used to climb out.
    #[test]
    fn new_names_cannot_climb_out_of_the_root() {
        let dir = tempdir().unwrap();
        for candidate in [
            "newdir/../../escaped.txt",
            "a/b/../../../escaped.txt",
            "../sibling/new.txt",
        ] {
            let got = resolve(dir.path(), Path::new(candidate), false);
            assert!(
                matches!(
                    got,
                    Err(PathError::Outside { .. }) | Err(PathError::NotFound { .. })
                ),
                "{candidate} was allowed: {got:?}"
            );
        }
    }

    /// A link inside the root pointing out of it must not become a usable base for new
    /// names underneath it.
    #[test]
    fn a_symlinked_directory_cannot_smuggle_new_names_outside() {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let link = dir.path().join("escape");
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(outside.path(), &link).unwrap();

        let got = resolve(dir.path(), Path::new("escape/new/file.txt"), false);
        assert!(
            matches!(got, Err(PathError::Outside { .. })),
            "a link out of the root became a base for new names: {got:?}"
        );
    }

    /// Writing through the resolved path must land inside the root - asserted on the
    /// consequence, like the dangling-link case above it.
    #[test]
    fn writing_through_a_resolved_new_path_stays_inside() {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let resolved = resolve(dir.path(), Path::new("a/b/c.txt"), false).unwrap();
        fs::create_dir_all(resolved.parent().unwrap()).unwrap();
        fs::write(&resolved, b"inside").unwrap();

        assert!(dir.path().join("a/b/c.txt").exists());
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    /// An existing file still resolves through its real identity, not its name.
    #[test]
    fn an_existing_path_is_unaffected() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/a.rs"), b"x").unwrap();
        let r = resolve(dir.path(), Path::new("src/a.rs"), false).unwrap();
        assert_eq!(r, dir.path().join("src/a.rs").canonicalize().unwrap());
    }
}

/// Paths under `root` whose file name matches the one that was not found.
///
/// A model that has not read the tree guesses a path, and the guess is usually right about
/// the file and wrong about where it sits: `/workspace/src/a.rs` for `src/a.rs`. Answering
/// the refusal with where the file actually is saves the round trip that would find it.
///
/// Bounded on both sides: the walk stops once enough matches are in hand, and the answer
/// names at most a few, because a refusal that lists a hundred paths is not an answer.
pub fn nearby(root: &Path, wanted: &Path) -> Vec<String> {
    const MOST: usize = 3;
    let Some(name) = wanted.file_name() else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in ignore::WalkBuilder::new(root).build().flatten() {
        if entry.file_name() != name {
            continue;
        }
        if let Ok(rel) = entry.path().strip_prefix(root) {
            found.push(rel.display().to_string());
            if found.len() == MOST {
                break;
            }
        }
    }
    found
}
