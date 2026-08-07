use std::path::{Path, PathBuf};

use thiserror::Error;

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
/// `must_exist = false` allows resolving a target file that hasn't been
/// created yet (write_file). In that case we canonicalise the parent
/// directory and join the file name.
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
                PathError::NotFound { path: absolute.clone() }
            } else {
                PathError::Io { path: absolute.clone(), source }
            }
        })?
    } else {
        // Parent must exist & canonicalise; the final filename can be new.
        let parent = absolute.parent().ok_or_else(|| PathError::NotFound {
            path: absolute.clone(),
        })?;
        let parent_canon = parent.canonicalize().map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                PathError::NotFound { path: parent.to_path_buf() }
            } else {
                PathError::Io { path: parent.to_path_buf(), source }
            }
        })?;
        let name = absolute
            .file_name()
            .ok_or_else(|| PathError::NotFound { path: absolute.clone() })?;
        parent_canon.join(name)
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
        assert!(matches!(err, PathError::Outside { .. } | PathError::NotFound { .. }));
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
