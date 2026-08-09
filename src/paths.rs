//! Where verve keeps things on disk.
//!
//! The XDG base directories, read from the environment where it says so and falling back
//! to the conventional places. Written out rather than taken from a crate: it is three
//! variables and a home directory, and the fallbacks are the whole of the specification
//! that matters here.

use std::path::PathBuf;

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn xdg(var: &str, default: &str) -> Option<PathBuf> {
    match std::env::var_os(var) {
        // The specification says a relative value is to be ignored, not resolved.
        Some(v) if PathBuf::from(&v).is_absolute() => Some(PathBuf::from(v)),
        _ => home().map(|h| h.join(default)),
    }
}

/// Logs and other state the user would not miss if it were deleted.
pub fn state_dir() -> Option<PathBuf> {
    xdg("XDG_STATE_HOME", ".local/state").map(|p| p.join("verve"))
}

/// Session transcripts: not disposable, since resuming depends on them.
pub fn data_dir() -> Option<PathBuf> {
    xdg("XDG_DATA_HOME", ".local/share").map(|p| p.join("verve"))
}

/// Where the session transcripts live.
pub fn sessions_dir() -> Option<PathBuf> {
    data_dir().map(|p| p.join("sessions"))
}

/// Default log file.
pub fn log_file() -> Option<PathBuf> {
    state_dir().map(|p| p.join("verve.log"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards the process-wide environment these functions read, so the cases below do
    /// not race each other.
    fn with_env<T>(vars: &[(&str, Option<&str>)], f: impl FnOnce() -> T) -> T {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _held = LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let saved: Vec<_> = vars
            .iter()
            .map(|(k, _)| (*k, std::env::var_os(k)))
            .collect();
        for (k, v) in vars {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        let out = f();
        for (k, v) in saved {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        out
    }

    #[test]
    fn an_absolute_xdg_value_is_used() {
        with_env(&[("XDG_STATE_HOME", Some("/tmp/xdg-state"))], || {
            assert_eq!(state_dir(), Some(PathBuf::from("/tmp/xdg-state/verve")));
        });
    }

    /// The specification says a relative value is invalid and must be ignored - not
    /// resolved against the working directory, which would scatter state through whatever
    /// project the agent happened to be pointed at.
    #[test]
    fn a_relative_xdg_value_is_ignored() {
        with_env(
            &[
                ("XDG_STATE_HOME", Some("relative/path")),
                ("HOME", Some("/somewhere")),
            ],
            || {
                assert_eq!(
                    state_dir(),
                    Some(PathBuf::from("/somewhere/.local/state/verve"))
                );
            },
        );
    }

    #[test]
    fn the_fallbacks_are_the_conventional_places() {
        with_env(
            &[
                ("XDG_STATE_HOME", None),
                ("XDG_DATA_HOME", None),
                ("HOME", Some("/somewhere")),
            ],
            || {
                assert_eq!(
                    log_file(),
                    Some(PathBuf::from("/somewhere/.local/state/verve/verve.log"))
                );
                assert_eq!(
                    sessions_dir(),
                    Some(PathBuf::from("/somewhere/.local/share/verve/sessions"))
                );
            },
        );
    }

    /// Nowhere to put anything is an answer, not a panic: the agent still works without a
    /// home directory, it just cannot remember anything afterwards.
    #[test]
    fn no_home_is_not_a_failure() {
        with_env(&[("XDG_STATE_HOME", None), ("HOME", None)], || {
            assert_eq!(state_dir(), None);
        });
    }
}
