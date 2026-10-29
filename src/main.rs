use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use lokode::{app, config};

/// Defaults, in one place, so that "not specified anywhere" has a single answer.
mod default {
    pub const BACKEND_URL: &str = "http://127.0.0.1:11435";
    pub const MAX_STEPS: usize = 16;
    /// Delegated turns at once. A local server answers them one after another, so more
    /// than this only holds windows open while they wait.
    pub const DELEGATES: usize = 2;
    /// The window to ask a local backend for.
    ///
    /// Large, and for a reason that is not the obvious one. A conversation that outgrows
    /// its window is compacted, compaction rewrites the history from near its start, and
    /// the server's hold on the prompt it had already read is a prefix - so every
    /// compaction makes the next turn re-read the whole conversation. A window the work
    /// fits in is therefore faster than a smaller one, not merely roomier: measured on one
    /// question against a thirty-billion-parameter model, twenty-four seconds here against
    /// three hundred at a quarter of it, with the turns after the first answering in a
    /// quarter of a second instead of a minute.
    ///
    /// A ceiling, not an assumption: a backend whose model list states a smaller widest
    /// window for the model is believed.
    pub const CONTEXT_TOKENS: usize = 131_072;

    /// What to reserve for one answer, capped at half the window in force.
    pub const MAX_ANSWER_TOKENS: usize = 4096;
}

// Flags carry no defaults of their own.
//
// A flag with a `default_value` cannot be told apart from one the user did not pass, and
// a setting that is always "specified" can never be overridden by a config file. The
// defaults live above and are applied last, once everything else has had its say.
#[derive(Parser, Debug)]
#[command(name = "lokode", version, about = "Local-first code agent TUI")]
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

    /// Where to write the log. Defaults to $XDG_STATE_HOME/lokode/lokode.log.
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
    /// `auto` asks the backend once, at startup, whether it really makes tool calls, and
    /// falls back to the JSON envelope when it refuses the field or takes it and ignores
    /// it. Pin either to skip the question.
    #[arg(long, value_enum)]
    tool_protocol: Option<app::ProtocolChoice>,

    /// How far the commands the agent runs are confined. Linux only. `workspace` when not
    /// given: writes in the project, the temporary directories and the build caches.
    #[arg(long, value_enum)]
    sandbox: Option<lokode::sandbox::confine::Level>,

    /// The earlier form of `--sandbox workspace`.
    #[arg(long, hide = true)]
    confine_shell: bool,

    /// Which API the backend speaks. Read off the URL when not given.
    #[arg(long, value_enum)]
    backend_kind: Option<config::BackendKind>,

    /// Take the project's hooks, servers, approvals and shell settings for this run
    /// without asking, as a script run in a project it knows does.
    #[arg(long)]
    trust_project: bool,

    /// Print where settings were read from, and what they resolved to, then exit.
    #[arg(long)]
    show_config: bool,

    /// Start planning: read and search, change nothing until /act.
    #[arg(long, conflicts_with = "accept_edits")]
    plan: bool,

    /// Start writing without asking. Commands and requests that leave the machine are
    /// still asked about.
    #[arg(long)]
    accept_edits: bool,

    /// Look into the project while nobody is waiting, and remember what is found.
    #[arg(long)]
    discover: bool,

    /// Run one turn without the interface and exit. Everything below applies to that.
    #[arg(short, long)]
    prompt: Option<String>,

    /// What a headless run may agree to, with nobody there to be asked.
    ///
    /// Read-only tools never prompt, so this governs the ones that change something, run
    /// something, or leave the machine. The default refuses all of them.
    #[arg(long, value_enum, default_value = "none", requires = "prompt")]
    approve: lokode::headless::ApprovePolicy,

    /// How a headless run reports. `text` streams prose to stdout and progress to stderr;
    /// `json` writes one object to stdout at the end.
    #[arg(long, value_enum, default_value = "text", requires = "prompt")]
    output: lokode::headless::OutputFormat,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Install, update, remove or list plugins.
    Plugin {
        #[command(subcommand)]
        action: PluginAction,
    },
}

#[derive(clap::Subcommand, Debug)]
enum PluginAction {
    /// Install a plugin from a git repository.
    Install {
        /// Where to clone it from.
        source: String,
        /// The name to install it under. The end of the source when not given.
        #[arg(long)]
        name: Option<String>,
        /// Install it without asking, even when it runs programs.
        #[arg(long)]
        yes: bool,
    },
    /// Bring plugins up to their source: those named, or every one installed from git.
    Update {
        names: Vec<String>,
        /// Take an update without asking, even when it runs new programs.
        #[arg(long)]
        yes: bool,
    },
    /// Take a plugin out.
    Remove { name: String },
    /// List the plugins installed.
    List,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(Command::Plugin { action }) = cli.command {
        return plugin_command(action);
    }

    let root = cli.root.canonicalize().map_err(|e| {
        anyhow::anyhow!(
            "--root {} could not be canonicalized: {e}",
            cli.root.display()
        )
    })?;
    let mut loaded = config::load(&root, cli.trust_project)?;
    if let Some(untrusted) = loaded.untrusted.clone() {
        if cli.prompt.is_none() && !cli.show_config && ask_trust(&untrusted)? {
            lokode::trust::grant(&root, &untrusted.guarded)?;
            loaded = config::load(&root, false)?;
        }
    }
    let config::Loaded { file, untrusted } = loaded;

    let log_file = cli
        .log_file
        .clone()
        .or_else(|| env_path("LOKODE_LOG_FILE"))
        .or_else(|| file.session.log_file.clone());
    let _log_guard = init_logging(log_file)?;

    let mut cfg = resolve(cli, root, file)?;
    if let Some(untrusted) = untrusted {
        let warning = untrusted.warning();
        tracing::warn!("{warning}");
        cfg.app.warnings.push(warning);
    }

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
            let code = runtime.block_on(lokode::headless::run(
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
    approve: lokode::headless::ApprovePolicy,
    output: lokode::headless::OutputFormat,
}

fn resolve(cli: Cli, root: PathBuf, file: config::FileConfig) -> Result<Resolved> {
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
        .or_else(|| env_string("LOKODE_BACKEND_URL"))
        .or_else(|| file.backend.url.clone())
        .unwrap_or_else(|| default::BACKEND_URL.to_string());

    let config::Headers {
        headers,
        mut warnings,
    } = config::resolve_headers(&file, &cli.headers, &backend_url)?;

    let starting_mode = match file.mode.as_deref() {
        None | Some("act") => "act",
        Some("plan") => "plan",
        Some("edits") => "edits",
        Some(other) => anyhow::bail!("mode must be plan, act or edits, got {other:?}"),
    };
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

    // Resolved before the rest so that asking for a confinement this machine cannot give
    // is news at startup, rather than the first command of the session failing.
    use lokode::sandbox::confine::Level;
    let asked = cli
        .sandbox
        .or(cli.confine_shell.then_some(Level::Workspace))
        .or(file.shell.level());
    let level = match asked {
        Some(level) => level,
        None if lokode::sandbox::confine::abi().is_some() => Level::Workspace,
        None => {
            warnings.push(
                "this system has no Landlock, so the commands the agent runs are not confined"
                    .into(),
            );
            Level::Off
        }
    };
    let sandbox =
        (level != Level::Off).then(|| config::resolve_sandbox(&file, &root, level, &mut warnings));
    let (lsp, lsp_warnings) = lokode::lsp::Languages::from_config(&file.lsp);
    warnings.extend(lsp_warnings);
    let (backends, backend_warnings) = config::resolve_backends(&file);
    warnings.extend(backend_warnings);
    let (agents, agent_warnings) = lokode::agent::agents::load(&lokode::agent::agents::dirs(&root));
    warnings.extend(agent_warnings);
    if sandbox.is_some() && lokode::sandbox::confine::abi().is_none() {
        warnings.push(
            "a sandbox was asked for and this system has no Landlock: commands will be \
             refused rather than run unconfined"
                .into(),
        );
    }

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
                .or_else(|| env_string("LOKODE_MODEL"))
                .or(file.model.clone()),
            root,
            backend_url,
            headers,
            grammar_enabled: !cli.no_grammar,
            max_steps: cli
                .max_steps
                .or(file.limits.max_steps)
                .unwrap_or(default::MAX_STEPS),
            context_was_set: cli.context_tokens.or(file.limits.context_tokens).is_some(),
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
            lsp,
            models: file.models.clone(),
            policy: file.approve.clone(),
            hooks: file.hooks.clone(),
            roles: file.roles.clone(),
            preload: file.preload.unwrap_or(true),
            speculate: file.speculate.unwrap_or(true),
            // Off unless asked for: it spends the machine's time on its own, and that is
            // a thing to opt into rather than to discover.
            discover: cli.discover || file.discover.unwrap_or(false),
            headless: cli.prompt.is_some(),
            mode: if cli.plan || (starting_mode == "plan" && !cli.accept_edits) {
                lokode::agent::agent_loop::Mode::Plan
            } else {
                lokode::agent::agent_loop::Mode::Act
            },
            parallel_delegates: file.limits.delegates.unwrap_or(default::DELEGATES),
            delegate_depth: file.limits.depth.unwrap_or(1),
            agents,
            backends,
            accept_writes: cli.accept_edits || (starting_mode == "edits" && !cli.plan),
            sandbox,
        },
    })
}

/// Ask on the terminal whether to trust the project's guarded settings. No when there is
/// no terminal to ask on.
/// Install, update, remove or list plugins, saying what was done.
fn plugin_command(action: PluginAction) -> Result<()> {
    use lokode::plugins;
    let home = plugins::home().context("there is no configuration directory")?;
    std::fs::create_dir_all(&home)?;
    match action {
        PluginAction::Install { source, name, yes } => {
            let name = name
                .or_else(|| plugins::name_for(&source))
                .context("no name can be read off the source: give one with --name")?;
            match plugins::install(&home, &source, &name, |p| {
                agree_plugin(p, yes, "Install it?")
            })? {
                Some(p) => println!(
                    "installed {} in {}: {}",
                    p.name,
                    p.dir.display(),
                    brought(&p)
                ),
                None => println!("{name} was not installed"),
            }
        }
        PluginAction::Update { names, yes } => {
            let names = match names.is_empty() {
                false => names,
                true => plugins::load_from(&home, &[])
                    .0
                    .into_iter()
                    .filter(|p| p.dir.join(".git").exists())
                    .map(|p| p.name)
                    .collect(),
            };
            if names.is_empty() {
                println!("no plugin was installed from git");
            }
            for name in names {
                match plugins::update(&home, &name, |p| {
                    agree_plugin(p, yes, "It runs something new. Take the update?")
                }) {
                    Ok(said) => println!("{said}"),
                    Err(e) => eprintln!("{name}: {e:#}"),
                }
            }
        }
        PluginAction::Remove { name } => {
            plugins::remove(&home, &name)?;
            println!("removed {name}");
        }
        PluginAction::List => {
            let disabled = config::user_config_path()
                .and_then(|p| config::read(&p).ok().flatten())
                .map(|c| c.plugins.disabled)
                .unwrap_or_default();
            let (installed, warnings) = plugins::load_from(&home, &disabled);
            if installed.is_empty() {
                println!("no plugin is installed in {}", home.display());
            }
            for p in installed {
                println!(
                    "{}{}  {}  {}",
                    p.name,
                    if p.disabled { " (off)" } else { "" },
                    p.file.description,
                    brought(&p)
                );
            }
            for w in warnings {
                eprintln!("{w}");
            }
        }
    }
    Ok(())
}

fn brought(p: &lokode::plugins::Plugin) -> String {
    match p.brings() {
        b if b.is_empty() => "nothing yet".to_string(),
        b => b.join(", "),
    }
}

/// Whether to take the plugin `p`: at once when it runs nothing or `yes` was given, by
/// asking on a terminal otherwise, and never without one.
fn agree_plugin(p: &lokode::plugins::Plugin, yes: bool, question: &str) -> bool {
    use std::io::{BufRead, IsTerminal, Write};
    let runs = p.file.runs();
    if runs.is_empty() || yes {
        return true;
    }
    let mut err = std::io::stderr().lock();
    let _ = writeln!(
        err,
        "{}: {}

It runs, with your rights:
",
        p.name, p.file.description
    );
    for line in &runs {
        let _ = writeln!(err, "    {line}");
    }
    if !std::io::stdin().is_terminal() {
        let _ = writeln!(
            err,
            "
Nobody is here to agree to it: run again with --yes to take it."
        );
        return false;
    }
    let _ = write!(
        err,
        "
{question} [y/N] "
    );
    let _ = err.flush();
    let mut answer = String::new();
    let _ = std::io::stdin().lock().read_line(&mut answer);
    matches!(answer.trim(), "y" | "Y" | "yes")
}

fn ask_trust(untrusted: &config::Untrusted) -> Result<bool> {
    use std::io::{BufRead, IsTerminal, Write};
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Ok(false);
    }
    let mut err = std::io::stderr().lock();
    writeln!(
        err,
        "{} asks for settings that run programs, loosen approvals or reach beyond the \
         project:\n",
        untrusted.path.display()
    )?;
    for line in untrusted.guarded.lines() {
        writeln!(err, "    {line}")?;
    }
    write!(
        err,
        "\nTrust them for this project? A change to them is asked about again. [y/N] "
    )?;
    err.flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
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
    println!(
        "  mode               {}",
        match a.accept_writes {
            true => "edits",
            false => a.mode.label(),
        }
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
        "  sandbox            {}",
        match &a.sandbox {
            None => "off".to_string(),
            Some(s) if s.writable.is_empty() => format!(
                "{}, network {}",
                s.level.label(),
                if s.network { "on" } else { "off" }
            ),
            Some(s) => format!(
                "{}, network {}, writes in {}",
                s.level.label(),
                if s.network { "on" } else { "off" },
                s.writable
                    .iter()
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
    let servers: Vec<_> = a
        .lsp
        .specs
        .iter()
        .map(|s| format!("{} ({})", s.language, s.command))
        .collect();
    println!("  language-servers   {}", servers.join(", "));
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
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn,lokode=info"));

    let Some(path) = explicit.or_else(lokode::paths::log_file) else {
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

    tracing::info!(path = %path.display(), "lokode starting");
    Ok(Some(guard))
}
