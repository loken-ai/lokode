use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

use verve::app;

#[derive(Parser, Debug)]
#[command(name = "verve", about = "Local-first code agent TUI")]
struct Cli {
    /// Model name as known to the backend (e.g. qwen3-coder:latest).
    #[arg(short, long)]
    model: String,

    /// Project root. All path-taking tools are confined under this directory.
    #[arg(short, long, default_value = ".")]
    root: PathBuf,

    /// Backend base URL (OpenAI-compatible).
    #[arg(short = 'u', long, default_value = "http://127.0.0.1:11435")]
    backend_url: String,

    /// Disable server-side grammar (forces Layer 2/3 repair to do all the work).
    #[arg(long)]
    no_grammar: bool,

    /// Maximum agent loop iterations per user turn.
    #[arg(long, default_value = "16")]
    max_steps: usize,

    /// Context window the backend offers for this model, in tokens. The history is
    /// compacted client-side to fit it: a server that has to truncate keeps the tail,
    /// which would drop the system preamble carrying the tool manifest.
    #[arg(long, default_value = "8192")]
    context_tokens: usize,

    /// Tokens held back from the window for the answer.
    #[arg(long, default_value = "4096")]
    max_answer_tokens: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,verve=info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let root = cli.root.canonicalize().map_err(|e| {
        anyhow::anyhow!("--root {} could not be canonicalized: {e}", cli.root.display())
    })?;

    let cfg = app::AppConfig {
        model: cli.model,
        root,
        backend_url: cli.backend_url,
        grammar_enabled: !cli.no_grammar,
        max_steps: cli.max_steps,
        context_tokens: cli.context_tokens,
        max_answer_tokens: cli.max_answer_tokens,
    };

    app::run(cfg).await
}
