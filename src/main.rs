use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use verve::app;

#[derive(Parser, Debug)]
#[command(name = "verve", about = "Local-first code agent TUI")]
struct Cli {
    /// Model name as known to the backend (e.g. qwen3-coder:latest).
    #[arg(short, long)]
    model: Option<String>,

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

    /// Let `web_fetch` reach this host even though it resolves to a private address.
    ///
    /// Egress is otherwise confined to public addresses, so that an approved public URL
    /// cannot redirect onto loopback or a cloud metadata endpoint. Repeat for each host.
    #[arg(long = "allow-host", value_name = "HOST")]
    allow_hosts: Vec<String>,

    /// Where to write the log. Defaults to $XDG_STATE_HOME/verve/verve.log.
    ///
    /// Never stderr: the TUI owns the screen, and writing to it would corrupt the display
    /// while destroying the log's own readability.
    #[arg(long, value_name = "PATH")]
    log_file: Option<PathBuf>,

    /// Do not record this session to disk.
    #[arg(long)]
    no_transcript: bool,

    /// Resume a recorded session by id.
    #[arg(long, value_name = "ID", conflicts_with = "continue_last")]
    resume: Option<String>,

    /// Resume the most recent session recorded for this root.
    #[arg(long = "continue", conflicts_with = "resume")]
    continue_last: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Installed before the runtime so that a panic during setup is reported on a terminal
    // nobody has taken yet, and after that by the hook the UI installs.
    let _log_guard = init_logging(cli.log_file.clone())?;

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(cli))
}

async fn run(cli: Cli) -> Result<()> {
    let root = cli.root.canonicalize().map_err(|e| {
        anyhow::anyhow!(
            "--root {} could not be canonicalized: {e}",
            cli.root.display()
        )
    })?;

    let resume = match (&cli.resume, cli.continue_last) {
        (Some(id), _) => Some(app::Resume::Id(id.clone())),
        (None, true) => Some(app::Resume::Latest),
        _ => None,
    };

    // Resuming can supply the model, so the flag is only required when nothing else can
    // answer for it.
    let cfg = app::AppConfig {
        model: cli.model,
        root,
        backend_url: cli.backend_url,
        grammar_enabled: !cli.no_grammar,
        max_steps: cli.max_steps,
        context_tokens: cli.context_tokens,
        max_answer_tokens: cli.max_answer_tokens,
        allow_hosts: cli.allow_hosts.into_iter().collect(),
        transcripts: !cli.no_transcript,
        resume,
    };

    app::run(cfg).await
}

/// Send the log to a file, and return the guard that keeps the writer alive.
///
/// The subscriber used to be pointed at stderr, which is the one place it must never go:
/// ratatui owns that screen. Between that and the fact that nothing in the codebase ever
/// emitted an event, `RUST_LOG` had no observable effect at all.
fn init_logging(
    explicit: Option<PathBuf>,
) -> Result<Option<tracing_appender::non_blocking::WorkerGuard>> {
    use tracing_subscriber::EnvFilter;

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn,verve=info"));

    let Some(path) = explicit.or_else(verve::paths::log_file) else {
        // No home directory to write into. Running without a log beats refusing to run.
        return Ok(None);
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating log directory {}", parent.display()))?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening log file {}", path.display()))?;

    let (writer, guard) = tracing_appender::non_blocking(file);
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(false)
        .init();

    tracing::info!(path = %path.display(), "verve starting");
    Ok(Some(guard))
}
