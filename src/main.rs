use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use verve::{app, config};

/// Defaults, in one place, so that "not specified anywhere" has a single answer.
mod default {
    pub const BACKEND_URL: &str = "http://127.0.0.1:11435";
    pub const MAX_STEPS: usize = 16;
    pub const CONTEXT_TOKENS: usize = 8192;
    pub const MAX_ANSWER_TOKENS: usize = 4096;
}

/// Flags carry no defaults of their own.
///
/// A flag with a `default_value` cannot be told apart from one the user did not pass, and
/// a setting that is always "specified" can never be overridden by a config file. The
/// defaults live above and are applied last, once everything else has had its say.
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
    #[arg(short = 'u', long)]
    backend_url: Option<String>,

    /// Extra header sent with every backend request, as `name=value`. Repeatable.
    #[arg(long = "header", value_name = "NAME=VALUE")]
    headers: Vec<String>,

    /// Disable server-side grammar (forces Layer 2/3 repair to do all the work).
    #[arg(long)]
    no_grammar: bool,

    /// Maximum agent loop iterations per user turn.
    #[arg(long)]
    max_steps: Option<usize>,

    /// Context window the backend offers for this model, in tokens. The history is
    /// compacted client-side to fit it: a server that has to truncate keeps the tail,
    /// which would drop the system preamble carrying the tool manifest.
    #[arg(long)]
    context_tokens: Option<usize>,

    /// Tokens held back from the window for the answer.
    #[arg(long)]
    max_answer_tokens: Option<usize>,

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

    /// How to ask the model for tool calls.
    ///
    /// `auto` uses the server's own tool-calling and falls back to the JSON envelope if it
    /// is refused. Pin `envelope` for a backend that accepts the `tools` field and then
    /// ignores it, which cannot be told apart from a model choosing not to call anything.
    #[arg(long, value_enum)]
    tool_protocol: Option<app::ProtocolChoice>,

    /// Make everything outside the project read-only for an approved shell command.
    ///
    /// Linux only. It can make a build fail where a cache lives outside the project, so
    /// name those directories with `[shell] writable` in the configuration.
    #[arg(long)]
    confine_shell: bool,

    /// Which API the backend speaks. Read off the URL when not given.
    #[arg(long, value_enum)]
    backend_kind: Option<config::BackendKind>,

    /// Print where settings were read from, and what they resolved to, then exit.
    #[arg(long)]
    show_config: bool,

    /// Run one turn without the interface and exit. Everything below applies to that.
    #[arg(short, long)]
    prompt: Option<String>,

    /// What a headless run may agree to, with nobody there to be asked.
    ///
    /// Read-only tools never prompt, so this governs the ones that change something, run
    /// something, or leave the machine. The default refuses all of them.
    #[arg(long, value_enum, default_value = "none", requires = "prompt")]
    approve: verve::headless::ApprovePolicy,

    /// How a headless run reports. `text` streams prose to stdout and progress to stderr;
    /// `json` writes one object to stdout at the end.
    #[arg(long, value_enum, default_value = "text", requires = "prompt")]
    output: verve::headless::OutputFormat,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let root = cli.root.canonicalize().map_err(|e| {
        anyhow::anyhow!(
            "--root {} could not be canonicalized: {e}",
            cli.root.display()
        )
    })?;
    let file = config::load(&root)?;

    let log_file = cli
        .log_file
        .clone()
        .or_else(|| env_path("VERVE_LOG_FILE"))
        .or_else(|| file.session.log_file.clone());
    let _log_guard = init_logging(log_file)?;

    let cfg = resolve(cli, root, file)?;

    if cfg.show_config {
        print_config(&cfg);
        return Ok(());
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    match cfg.prompt {
        // A headless run answers with an exit code, because the caller is a script.
        Some(prompt) => {
            let code = runtime.block_on(verve::headless::run(
                cfg.app,
                prompt,
                cfg.approve,
                cfg.output,
            ))?;
            std::process::exit(code);
        }
        None => runtime.block_on(app::run(cfg.app)),
    }
}

/// Everything resolved, plus what the user should be told about it.
struct Resolved {
    app: app::AppConfig,
    show_config: bool,
    sources: Vec<String>,
    /// Present when this is a headless run.
    prompt: Option<String>,
    approve: verve::headless::ApprovePolicy,
    output: verve::headless::OutputFormat,
}

fn resolve(cli: Cli, root: PathBuf, file: config::FileConfig) -> Result<Resolved> {
    let root_for_shell = root.clone();
    let mut sources = Vec::new();
    if let Some(p) = config::user_config_path() {
        if p.exists() {
            sources.push(p.display().to_string());
        }
    }
    let project = config::project_config_path(&root);
    if project.exists() {
        sources.push(project.display().to_string());
    }

    let backend_url = cli
        .backend_url
        .or_else(|| env_string("VERVE_BACKEND_URL"))
        .or_else(|| file.backend.url.clone())
        .unwrap_or_else(|| default::BACKEND_URL.to_string());

    let config::Headers {
        headers,
        mut warnings,
    } = config::resolve_headers(&file, &cli.headers, &backend_url)?;

    let tool_protocol = match cli.tool_protocol {
        Some(p) => p,
        None => match file.tool_protocol.as_deref() {
            None | Some("auto") => app::ProtocolChoice::Auto,
            Some("native") => app::ProtocolChoice::Native,
            Some("envelope") => app::ProtocolChoice::Envelope,
            Some(other) => {
                anyhow::bail!("tool-protocol must be auto, native or envelope, got {other:?}")
            }
        },
    };

    // Settled before the URL is moved into the config, and settled once: `auto` reads the
    // kind off the URL, so a later reader sees a decision rather than a question.
    let backend_kind = cli
        .backend_kind
        .or(file.backend.kind)
        .unwrap_or_default()
        .resolve(&backend_url);

    let resume = match (&cli.resume, cli.continue_last) {
        (Some(id), _) => Some(app::Resume::Id(id.clone())),
        (None, true) => Some(app::Resume::Latest),
        _ => None,
    };

    // A flag that was not passed is silence, not a "no": only `--no-transcript` and
    // `--no-grammar` being present count as an opinion, so a file can still turn either
    // off without the flag having to be repeated.
    let transcripts = if cli.no_transcript {
        false
    } else {
        file.session.transcripts.unwrap_or(true)
    };

    if warnings.is_empty() && headers.iter().any(|(k, _)| k == "Authorization") {
        tracing::info!("sending an Authorization header to {backend_url}");
    }
    for w in &warnings {
        tracing::warn!("{w}");
    }
    warnings.shrink_to_fit();

    Ok(Resolved {
        show_config: cli.show_config,
        sources,
        prompt: cli.prompt.clone(),
        approve: cli.approve,
        output: cli.output,
        app: app::AppConfig {
            model: cli
                .model
                .or_else(|| env_string("VERVE_MODEL"))
                .or(file.model.clone()),
            root,
            backend_url,
            headers,
            grammar_enabled: !cli.no_grammar,
            max_steps: cli
                .max_steps
                .or(file.limits.max_steps)
                .unwrap_or(default::MAX_STEPS),
            context_tokens: cli
                .context_tokens
                .or(file.limits.context_tokens)
                .unwrap_or(default::CONTEXT_TOKENS),
            max_answer_tokens: cli
                .max_answer_tokens
                .or(file.limits.max_answer_tokens)
                .unwrap_or(default::MAX_ANSWER_TOKENS),
            allow_hosts: config::resolve_allow_hosts(&file, cli.allow_hosts),
            transcripts,
            resume,
            tool_protocol,
            backend_kind,
            warnings,
            mcp_servers: file.mcp.servers.clone(),
            // The project itself is always writable; the rest is what was named.
            shell_writable: (cli.confine_shell || file.shell.confine_writes.unwrap_or(false)).then(
                || {
                    let mut dirs = vec![root_for_shell.clone()];
                    dirs.extend(file.shell.writable.iter().cloned());
                    dirs
                },
            ),
        },
    })
}

fn env_string(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn env_path(name: &str) -> Option<PathBuf> {
    env_string(name).map(PathBuf::from)
}

/// Say what was read and what it came to, without ever printing a secret.
fn print_config(cfg: &Resolved) {
    if cfg.sources.is_empty() {
        println!("config files: none found");
    } else {
        println!("config files (later wins):");
        for s in &cfg.sources {
            println!("  {s}");
        }
    }
    let a = &cfg.app;
    println!("\nresolved:");
    println!(
        "  model              {}",
        a.model.as_deref().unwrap_or("(unset)")
    );
    println!("  root               {}", a.root.display());
    println!("  backend-url        {}", a.backend_url);
    println!("  backend-kind       {}", a.backend_kind.label());
    println!("  tool-protocol      {:?}", a.tool_protocol);
    println!("  grammar            {}", a.grammar_enabled);
    println!("  max-steps          {}", a.max_steps);
    println!("  context-tokens     {}", a.context_tokens);
    println!("  max-answer-tokens  {}", a.max_answer_tokens);
    println!("  transcripts        {}", a.transcripts);
    println!(
        "  shell writes       {}",
        match &a.shell_writable {
            None => "unconfined".to_string(),
            Some(dirs) => format!(
                "confined to {}",
                dirs.iter()
                    .map(|d| d.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    );
    let hosts: Vec<_> = a.allow_hosts.iter().cloned().collect();
    println!("  allow-hosts        {}", hosts.join(", "));
    // Names only. The whole point of the file-permission check is that these do not get
    // shown to whoever is looking over your shoulder either.
    let names: Vec<_> = a.headers.iter().map(|(k, _)| k.as_str()).collect();
    println!("  headers            {} (values hidden)", names.join(", "));
    for w in &a.warnings {
        println!("\nwarning: {w}");
    }
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
