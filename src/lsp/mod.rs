//! Language servers: one per language the project uses, started when first needed.
//!
//! A server that stops is started again once. A second stop, or a server that cannot be
//! started at all, leaves it down for the session with what it said, so a tool asks once
//! and is answered at once afterwards.

pub mod check;
pub mod language;
pub mod position;
pub mod server;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;
use tokio::sync::Mutex;

pub use language::{Languages, ServerSpec};
pub use server::Server;

/// How many times a server that stopped is started again in one session.
const RESTARTS: u32 = 1;

enum Slot {
    Up { server: Arc<Server>, restarts: u32 },
    Down { why: String },
}

/// The language servers of one session.
pub struct LspPool {
    root: PathBuf,
    languages: Languages,
    slots: Mutex<HashMap<String, Slot>>,
    /// The languages the project uses that have a server installed, most used first.
    available: Vec<String>,
    /// The languages the project uses whose server is not installed, with its command.
    missing: Vec<(String, String)>,
}

impl LspPool {
    pub fn new(root: PathBuf) -> Arc<Self> {
        Self::with(root, Languages::builtin())
    }

    /// The servers of `languages` for the project at `root`, with the languages it uses
    /// read from its files.
    pub fn with(root: PathBuf, languages: Languages) -> Arc<Self> {
        let (available, missing) = language::present(&root, &languages)
            .into_iter()
            .filter_map(|l| languages.named(&l).cloned())
            .partition::<Vec<_>, _>(|spec| language::installed(&spec.command));
        Arc::new(Self {
            root,
            languages,
            slots: Mutex::new(HashMap::new()),
            available: available.into_iter().map(|s| s.language).collect(),
            missing: missing
                .into_iter()
                .map(|s| (s.language, s.command))
                .collect(),
        })
    }

    /// The languages the project uses that have a server installed, most used first.
    pub fn available(&self) -> &[String] {
        &self.available
    }

    /// The languages the project uses whose server is not installed, with its command.
    pub fn missing(&self) -> &[(String, String)] {
        &self.missing
    }

    /// Start the servers of the project's languages in the background, so they read the
    /// project while nobody waits on them.
    pub fn warm(self: &Arc<Self>) {
        for language in self.available.clone() {
            let pool = self.clone();
            tokio::spawn(async move {
                if let Some(spec) = pool.languages.named(&language).cloned() {
                    let _ = pool.ensure(&spec).await;
                }
            });
        }
    }

    /// The server that answers for `path`, started if it is not running, or why there is
    /// none.
    pub async fn server_for(&self, path: &Path) -> Result<Arc<Server>, String> {
        let spec = self
            .languages
            .for_path(path)
            .ok_or_else(|| format!("no language server is known for {}", path.display()))?
            .clone();
        self.ensure(&spec).await
    }

    /// The server of `spec`, started if it is not running, or why there is none.
    async fn ensure(&self, spec: &ServerSpec) -> Result<Arc<Server>, String> {
        let mut slots = self.slots.lock().await;
        let restarts = match slots.get(&spec.language) {
            Some(Slot::Up { server, .. }) if server.alive() => return Ok(server.clone()),
            Some(Slot::Down { why }) => return Err(why.clone()),
            Some(Slot::Up { server, restarts }) if *restarts >= RESTARTS => {
                let why = format!(
                    "{} stopped {} times and is left down for this session{}",
                    spec.command,
                    restarts + 1,
                    server.stderr_tail().await
                );
                slots.insert(spec.language.clone(), Slot::Down { why: why.clone() });
                return Err(why);
            }
            Some(Slot::Up { restarts, .. }) => {
                tracing::warn!(server = %spec.command, "language server stopped; starting it again");
                restarts + 1
            }
            None => 0,
        };
        match Server::spawn(spec, &self.root).await {
            Ok(server) => {
                slots.insert(
                    spec.language.clone(),
                    Slot::Up {
                        server: server.clone(),
                        restarts,
                    },
                );
                Ok(server)
            }
            Err(e) => {
                let why = format!("{e:#}");
                tracing::warn!(server = %spec.command, error = %why, "language server is down");
                slots.insert(spec.language.clone(), Slot::Down { why: why.clone() });
                Err(why)
            }
        }
    }

    /// Whether a server is known for `path`.
    pub fn knows(&self, path: &Path) -> bool {
        self.languages.for_path(path).is_some()
    }

    /// The servers running now.
    pub async fn running(&self) -> Vec<Arc<Server>> {
        self.slots
            .lock()
            .await
            .values()
            .filter_map(|s| match s {
                Slot::Up { server, .. } if server.alive() => Some(server.clone()),
                _ => None,
            })
            .collect()
    }

    /// Tell every running server that `paths` were written or removed: created when it
    /// never had the file open and the file is there, deleted when the file is gone.
    pub async fn files_changed(&self, paths: &[PathBuf]) {
        for server in self.running().await {
            let opened = server.opened.lock().await;
            let changes: Vec<_> = paths
                .iter()
                .filter_map(|p| {
                    let uri = lsp_types::Url::from_file_path(p).ok()?;
                    let kind = match (p.exists(), opened.contains_key(p)) {
                        (true, false) => 1,
                        (true, true) => 2,
                        (false, _) => 3,
                    };
                    Some(json!({"uri": uri, "type": kind}))
                })
                .collect();
            drop(opened);
            if !changes.is_empty() {
                let _ = server
                    .notify(
                        "workspace/didChangeWatchedFiles",
                        json!({"changes": changes}),
                    )
                    .await;
            }
        }
    }

    /// Ask every running server to stop.
    pub async fn shutdown(&self) {
        let servers: Vec<_> = self
            .slots
            .lock()
            .await
            .drain()
            .filter_map(|(_, s)| match s {
                Slot::Up { server, .. } => Some(server),
                Slot::Down { .. } => None,
            })
            .collect();
        for server in servers {
            server.shutdown().await;
        }
    }
}
