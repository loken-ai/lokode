use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use anyhow::Result;

use crate::ui;

/// Which recorded session to pick up.
#[derive(Debug, Clone)]
pub enum Resume {
    Id(String),
    /// The most recent one recorded against this root.
    Latest,
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    /// `None` when the model is to come from the session being resumed.
    pub model: Option<String>,
    pub root: PathBuf,
    pub backend_url: String,
    /// Which API the backend speaks, already settled against the URL.
    pub backend_kind: crate::config::BackendKind,
    /// Sent with every backend request, including any `Authorization` resolved from the
    /// environment or a config file.
    pub headers: Vec<(String, String)>,
    pub grammar_enabled: bool,
    pub max_steps: usize,
    pub context_tokens: usize,
    pub max_answer_tokens: usize,
    /// Hosts `web_fetch` may reach even though they resolve to a private address.
    pub allow_hosts: HashSet<String>,
    /// Whether to record this session to disk.
    pub transcripts: bool,
    pub resume: Option<Resume>,
    pub tool_protocol: ProtocolChoice,
    /// Things worth telling the user at startup, shown once the screen exists.
    pub warnings: Vec<String>,
    /// Tool servers to start, by name.
    pub mcp_servers: BTreeMap<String, crate::config::McpServer>,
    /// What each named model needs, when it differs from the defaults.
    pub models: BTreeMap<String, crate::config::ModelSection>,
    /// Whether the session starts able to change things.
    pub mode: crate::agent::agent_loop::Mode,
    /// What is settled in advance about approvals.
    pub policy: crate::sandbox::policy::Policy,
    /// Commands to run at the edges of a tool call and a turn.
    pub hooks: crate::config::HooksSection,
    /// Whether this run is driven by a script rather than by a person at a screen.
    pub headless: bool,
    /// Directories an approved shell command may write to, or `None` to leave it
    /// unconfined.
    pub shell_writable: Option<Vec<PathBuf>>,
}

/// How tools should be asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ProtocolChoice {
    /// Try the server's own tool-calling, and drop to the envelope if it refuses.
    Auto,
    /// Insist on the server's tool-calling.
    Native,
    /// A single JSON object per turn, constrained by a generated grammar. Needs nothing
    /// from the server beyond a completion.
    Envelope,
}

pub async fn run(cfg: AppConfig) -> Result<()> {
    ui::run(cfg).await
}
