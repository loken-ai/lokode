use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::Result;

use crate::ui;

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub model: String,
    pub root: PathBuf,
    pub backend_url: String,
    pub grammar_enabled: bool,
    pub max_steps: usize,
    pub context_tokens: usize,
    pub max_answer_tokens: usize,
    /// Hosts `web_fetch` may reach even though they resolve to a private address.
    pub allow_hosts: HashSet<String>,
}

pub async fn run(cfg: AppConfig) -> Result<()> {
    ui::run(cfg).await
}
