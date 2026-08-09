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
    pub grammar_enabled: bool,
    pub max_steps: usize,
    pub context_tokens: usize,
    pub max_answer_tokens: usize,
    /// Hosts `web_fetch` may reach even though they resolve to a private address.
    pub allow_hosts: HashSet<String>,
    /// Whether to record this session to disk.
    pub transcripts: bool,
    pub resume: Option<Resume>,
}

pub async fn run(cfg: AppConfig) -> Result<()> {
    ui::run(cfg).await
}
