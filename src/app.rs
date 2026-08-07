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
}

pub async fn run(cfg: AppConfig) -> Result<()> {
    ui::run(cfg).await
}
