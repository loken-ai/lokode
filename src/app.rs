use std::collections::HashSet;
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
