use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use lokode::{app, config};

/// Defaults, in one place, so that "not specified anywhere" has a single answer.
mod default {
    pub const BACKEND_URL: &str = "http://127.0.0.1:11435";
    pub const MAX_STEPS: usize = 16;
    /// Delegated turns at once.
    ///
    /// A local server answers them one after another.
    /// More than this only holds windows open while they wait.
    pub const DELEGATES: usize = 2;
    /// The window to ask a local backend for.
    ///
    /// A conversation that outgrows its window is compacted.
    /// Compaction rewrites the history from near its start.
    /// The server only reuses the prompt it already read as a prefix.
    /// So every compaction makes the next turn re-read the whole conversation.
    /// A window the work fits in avoids that cost.
    ///
    /// This is a ceiling.
    /// A backend whose model list states a smaller widest window for the model wins.
    pub const CONTEXT_TOKENS: usize = 131_072;

    /// What to reserve for one answer, capped at half the window in force.
    pub const MAX_ANSWER_TOKENS: usize = 4096;
}

// Flags carry no defaults of their own.
//
// A flag with a `default_value` cannot be told apart from one the user did not pass.
// A setting that is always "specified" can never be overridden by a config file.
// The defaults live above and are applied last, once everything else has had its say.
#[derive(Parser, Debug, Clone)]
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

    /// A provider lokode knows by name, in place of the configuration's [backend]:
    /// openai, anthropic, openrouter, gemini, mistral, groq, ollama and others.
    #[arg(long)]
    provider: Option<String>,

    /// Extra header sent with every backend request, as `name=value`. Repeatable.
    #[arg(long = "header", value_name = "NAME=VALUE")]
    headers: Vec<String>,

    /// Disable server-side grammar (forces Layer 2/3 repair to do all the work).
    #[arg(long)]
    no_grammar: bool,

    /// Stop each turn at this many steps.
    /// Unset, a turn goes on while it changes files or makes calls it has not made, and stops
    /// after as many steps without either as its first change took.
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

    /// How far the commands the agent runs are confined. Linux, macOS and Windows, where only writes are
    /// confined. `workspace` when not given: writes in the project, the temporary directories and the build caches.
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

#[derive(clap::Subcommand, Debug, Clone)]
enum Command {
    /// Serve an editor over the Agent Client Protocol, on standard input and output.
    Acp,
    /// Draw a session of a running `lokode serve` in this terminal.
    Attach {
        /// The address lokode serve printed, with its token.
        url: String,
        /// The session to follow. The latest one open, or a new one, when not given.
        #[arg(long)]
        session: Option<String>,
    },
    /// Serve the project's sessions to a browser page, on this machine only.
    Serve {
        /// The port on 127.0.0.1. Any free one when not given.
        #[arg(long)]
        port: Option<u16>,
        /// Share the sessions with the dashboards of the group named under [dashboard].
        /// The server then listens on the network, which it does not otherwise.
        /// It takes the address to listen on, as `ip:port`, so that a dashboard on another
        /// network can name it in `seeds`. Without one, every interface and any free port.
        #[arg(long, num_args = 0..=1, default_missing_value = "0.0.0.0:0", value_name = "ADDRESS")]
        share: Option<std::net::SocketAddr>,
    },
    /// Carry standard input and output to a host through the proxy of the session.
    /// For a client that does not read the proxy variables, as `ssh -o ProxyCommand` runs it.
    Tunnel { host: String, port: u16 },
    /// Run a command at the low integrity level, in a job of its own. Windows only.
    /// The session runs it in front of the shell, and a person has no use for it.
    #[command(hide = true)]
    ConfineExec {
        /// A directory the command may write to.
        #[arg(long = "writable")]
        writable: Vec<std::path::PathBuf>,
        /// The program and its arguments.
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Show the sessions of every lokode server of the group named under [dashboard].
    Dashboard,
    /// Install, update, remove or list plugins.
    Plugin {
        #[command(subcommand)]
        action: PluginAction,
    },
}

#[derive(clap::Subcommand, Debug, Clone)]
enum PluginAction {
    /// Install a plugin from a git repository, or one a marketplace lists.
    Install {
        /// A git url or path to clone, or a listed plugin as name or name@marketplace.
        source: String,
        /// The name to install it under. The end of the source when not given.
        #[arg(long)]
        name: Option<String>,
        /// Install it without asking, even when it runs programs.
        #[arg(long)]
        yes: bool,
    },
    /// Bring plugins up to their source: those named, or every one that has a source.
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
    /// List what the marketplaces offer, those whose name or description holds `term`.
    Search { term: Option<String> },
    /// Add, update, remove or list marketplaces.
    Marketplace {
        #[command(subcommand)]
        action: MarketAction,
    },
}

#[derive(clap::Subcommand, Debug, Clone)]
enum MarketAction {
    /// Add the marketplace kept in a git repository.
    Add {
        url: String,
        /// The name to add it under. The end of the url when not given.
        #[arg(long)]
        name: Option<String>,
    },
    /// Bring marketplaces up to their source: those named, or all of them.
    Update { names: Vec<String> },
    /// Take a marketplace out. The plugins installed from it stay.
    Remove { name: String },
    /// List the marketplaces added.
    List,
}

fn main() -> Result<()> {
    let mut cli = Cli::parse();
    match cli.command.take() {
        Some(Command::Plugin { action }) => return plugin_command(action),
        Some(Command::Acp) => return acp_command(cli, None, None),
        Some(Command::Serve { port, share }) => {
            return acp_command(cli, Some(port.unwrap_or(0)), share)
        }
        Some(Command::Attach { url, session }) => return attach_command(cli, url, session),
        Some(Command::Tunnel { host, port }) => return tunnel_command(&host, port),
        Some(Command::Dashboard) => return dashboard_command(cli),
        Some(Command::ConfineExec { writable, command }) => {
            return confine_exec_command(&writable, &command)
        }
        None => {}
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
    lokode::sound::declare(file.sounds.clone());

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
        // A headless run answers with an exit code.
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

    // A provider named on the command line takes the place of the configured backend.
    // Nothing written for another provider, its key included, is sent to this one.
    let mut file = file;
    if let Some(provider) = &cli.provider {
        file.backend = config::BackendSection {
            provider: Some(provider.clone()),
            ..Default::default()
        };
    }
    file.backend = config::with_preset(&file.backend).map_err(anyhow::Error::msg)?;

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

    // Settled once, before the URL is moved into the config.
    // `auto` reads the kind off the URL, so a later reader sees a decision.
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

    // A flag that was not passed is silence, not a "no".
    // Only `--no-transcript` and `--no-grammar` being present count as an opinion.
    // So a file can still turn either off without the flag having to be repeated.
    let transcripts = if cli.no_transcript {
        false
    } else {
        file.session.transcripts.unwrap_or(true)
    };

    // Resolved before the rest.
    // Asking for a confinement this machine cannot give is reported at startup.
    use lokode::sandbox::confine::Level;
    let asked = cli
        .sandbox
        .or(cli.confine_shell.then_some(Level::Workspace))
        .or(file.shell.level());
    let level = match asked {
        Some(level) => level,
        None if lokode::sandbox::confine::available() => Level::Workspace,
        None => {
            warnings.push(format!(
                "this system has no {}, so the commands the agent runs are not confined",
                lokode::sandbox::confine::MECHANISM
            ));
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
    if sandbox.is_some() && !lokode::sandbox::confine::available() {
        warnings.push(format!(
            "a sandbox was asked for and this system has no {}: commands will be refused \
             rather than run unconfined",
            lokode::sandbox::confine::MECHANISM
        ));
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
            remote: false,
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
            max_steps_was_set: cli.max_steps.or(file.limits.max_steps).is_some(),
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
            // Off unless asked for: it spends the machine's time on its own.
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

/// Serve an editor, or with `port` the pages of a browser.
///
/// Each session is configured for its project as the terminal would be.
/// A project's guarded settings wait for trust, since nobody can be asked here.
fn acp_command(cli: Cli, port: Option<u16>, share: Option<std::net::SocketAddr>) -> Result<()> {
    let root = cli.root.canonicalize().map_err(|e| {
        anyhow::anyhow!(
            "--root {} could not be canonicalized: {e}",
            cli.root.display()
        )
    })?;
    let _log_guard = init_logging(cli.log_file.clone().or_else(|| env_path("LOKODE_LOG_FILE")))?;
    let config_for: lokode::acp::ConfigFor = std::sync::Arc::new(move |root: &std::path::Path| {
        let config::Loaded { file, untrusted } = config::load(root, cli.trust_project)?;
        let mut resolved = resolve(cli.clone(), root.to_path_buf(), file)?;
        if let Some(untrusted) = untrusted {
            resolved.app.warnings.push(format!(
                "{} Run lokode in a terminal in this project once to trust them, or give the \
                 editor --trust-project.",
                untrusted.warning()
            ));
        }
        Ok(resolved.app)
    });
    let group = match share {
        Some(listen) => Some((
            config::load(&root, false)?
                .file
                .dashboard
                .group()
                .map_err(anyhow::Error::msg)?,
            listen,
        )),
        None => None,
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    match port {
        Some(port) => runtime.block_on(lokode::serve::run(config_for, root, port, group)),
        None => runtime.block_on(lokode::acp::run(config_for)),
    }
}

/// Draw a session of the `lokode serve` at `url` in this terminal.
fn attach_command(cli: Cli, url: String, session: Option<String>) -> Result<()> {
    let _log_guard = init_logging(cli.log_file.clone().or_else(|| env_path("LOKODE_LOG_FILE")))?;
    let root = cli.root.canonicalize()?;
    let config::Loaded { file, .. } = config::load(&root, false)?;
    let resolved = resolve(cli, root, file)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(lokode::ui::app_loop::run_remote(resolved.app, url, session))
}

/// Carry this process's standard input and output to `host:port` through the session's proxy.
///
/// The reason for a refusal goes to standard error, where the client that ran it shows it.
fn tunnel_command(host: &str, port: u16) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    if let Err(why) = runtime.block_on(lokode::sandbox::tunnel::run(host, port)) {
        eprintln!("lokode tunnel: {why}");
        std::process::exit(1);
    }
    Ok(())
}

/// Show the sessions of the servers of the group in this terminal.
fn dashboard_command(cli: Cli) -> Result<()> {
    let _log_guard = init_logging(cli.log_file.clone().or_else(|| env_path("LOKODE_LOG_FILE")))?;
    let root = cli.root.canonicalize()?;
    let config::Loaded { file, .. } = config::load(&root, false)?;
    let section = file.dashboard.clone();
    let group = section.group().map_err(anyhow::Error::msg)?;
    let (seeds, lost) = lokode::dashboard::resolve_seeds(&section.seeds);
    for seed in lost {
        eprintln!("lokode dashboard: the seed {seed} does not resolve");
    }
    let app = resolve(cli, root, file)?.app;
    let open: lokode::dashboard::Opener = Box::new(move |url, session| {
        let app = app.clone();
        Box::pin(lokode::ui::app_loop::run_remote(app, url, Some(session)))
    });
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(lokode::dashboard::run(group, seeds, open))
}

/// Run a command at the low integrity level, and end with its exit code.
fn confine_exec_command(writable: &[std::path::PathBuf], command: &[String]) -> Result<()> {
    #[cfg(windows)]
    {
        let (program, args) = command
            .split_first()
            .context("there is no command to run")?;
        match lokode::sandbox::winbox::run(writable, program, args) {
            Ok(code) => std::process::exit(code),
            Err(why) => {
                eprintln!("lokode confine-exec: {why}");
                std::process::exit(126);
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (writable, command);
        anyhow::bail!("confine-exec runs on Windows only")
    }
}

/// Install, update, remove or list plugins, saying what was done.
fn plugin_command(action: PluginAction) -> Result<()> {
    use lokode::{marketplace, plugins};
    let home = plugins::home().context("there is no configuration directory")?;
    std::fs::create_dir_all(&home)?;
    match action {
        PluginAction::Install { source, name, yes } => {
            let agree = |p: &plugins::Plugin| agree_plugin(p, yes, "Install it?");
            // A url or a path is cloned.
            // Anything else is a name a marketplace lists.
            let cloned = source.contains("://")
                || source.starts_with("git@")
                || source.contains('/')
                || source.ends_with(".git");
            let installed = match cloned {
                true => {
                    let name = name
                        .clone()
                        .or_else(|| plugins::name_for(&source))
                        .context("no name can be read off the source: give one with --name")?;
                    plugins::install(&home, &source, &name, agree)?
                }
                false => {
                    let markets = load_markets()?;
                    let listing = marketplace::find(&markets, &source)?;
                    let name = name.clone().unwrap_or_else(|| listing.name.clone());
                    match &listing.source {
                        marketplace::Source::Git(url) => {
                            plugins::install(&home, url, &name, agree)?
                        }
                        marketplace::Source::Dir(dir) => {
                            plugins::install_dir(&home, dir, &listing.market, &name, agree)?
                        }
                        other => {
                            plugins::install_source(&home, other, &listing.market, &name, agree)?
                        }
                    }
                }
            };
            let name = name.unwrap_or(source);
            match installed {
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
                    .filter(|p| {
                        p.dir.join(".git").exists() || plugins::market_of(&home, &p.name).is_some()
                    })
                    .map(|p| p.name)
                    .collect(),
            };
            if names.is_empty() {
                println!("no plugin has a source to update it from");
            }
            // Each marketplace a plugin came from is brought up to date once, first.
            let market_home = marketplace::home().context("there is no configuration directory")?;
            let mut refreshed = std::collections::BTreeMap::new();
            for name in names {
                let agree = |p: &plugins::Plugin| {
                    agree_plugin(p, yes, "It runs something new. Take the update?")
                };
                let said = match plugins::market_of(&home, &name) {
                    None => plugins::update(&home, &name, agree),
                    Some(market) => {
                        let fresh = refreshed
                            .entry(market.clone())
                            .or_insert_with(|| marketplace::update(&market_home, &market));
                        match fresh {
                            Err(e) => Err(anyhow::anyhow!("the marketplace {market}: {e:#}")),
                            Ok(m) => match marketplace::find(
                                std::slice::from_ref(m),
                                &format!("{name}@{market}"),
                            ) {
                                Ok(marketplace::Listing {
                                    source: marketplace::Source::Dir(dir),
                                    ..
                                }) => plugins::update_from_dir(&home, &dir, &name, agree),
                                Ok(marketplace::Listing {
                                    source: marketplace::Source::Git(_),
                                    ..
                                }) => Err(anyhow::anyhow!(
                                    "{market} now lists {name} from git: remove it and install it again"
                                )),
                                Ok(listing) => {
                                    plugins::update_source(&home, &listing.source, &name, agree)
                                }
                                Err(e) => Err(e),
                            },
                        }
                    }
                };
                match said {
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
        PluginAction::Search { term } => {
            let markets = load_markets()?;
            if markets.is_empty() {
                println!("no marketplace is added: lokode plugin marketplace add <git url>");
            }
            let term = term.unwrap_or_default().to_lowercase();
            for l in markets.iter().flat_map(|m| &m.listings) {
                let text = format!("{} {}", l.name, l.description).to_lowercase();
                if !text.contains(&term) {
                    continue;
                }
                let installed = home.join(&l.name).join("plugin.toml").is_file();
                println!(
                    "{}@{}{}{}  {}",
                    l.name,
                    l.market,
                    l.version
                        .as_deref()
                        .map(|v| format!(" {v}"))
                        .unwrap_or_default(),
                    if installed { " (installed)" } else { "" },
                    l.description
                );
            }
        }
        PluginAction::Marketplace { action } => {
            let market_home = marketplace::home().context("there is no configuration directory")?;
            match action {
                MarketAction::Add { url, name } => {
                    let m = marketplace::add(&market_home, &url, name.as_deref())?;
                    println!(
                        "added the marketplace {}: {} plugin(s). lokode plugin search lists them",
                        m.name,
                        m.listings.len()
                    );
                }
                MarketAction::Update { names } => {
                    let names = match names.is_empty() {
                        false => names,
                        true => load_markets()?.into_iter().map(|m| m.name).collect(),
                    };
                    for name in names {
                        match marketplace::update(&market_home, &name) {
                            Ok(m) => println!("{name}: {} plugin(s)", m.listings.len()),
                            Err(e) => eprintln!("{name}: {e:#}"),
                        }
                    }
                }
                MarketAction::Remove { name } => {
                    marketplace::remove(&market_home, &name)?;
                    println!("removed the marketplace {name}; its plugins stay installed");
                }
                MarketAction::List => {
                    let markets = load_markets()?;
                    if markets.is_empty() {
                        println!("no marketplace is added");
                    }
                    for m in markets {
                        println!(
                            "{}  {}  {} plugin(s)",
                            m.name,
                            m.description,
                            m.listings.len()
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

/// The marketplaces added, with a word on any that could not be read.
fn load_markets() -> Result<Vec<lokode::marketplace::Market>> {
    let home = lokode::marketplace::home().context("there is no configuration directory")?;
    let (markets, warnings) = lokode::marketplace::load(&home);
    for w in warnings {
        eprintln!("{w}");
    }
    Ok(markets)
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
    match a.max_steps_was_set {
        true => println!("  max-steps          {}", a.max_steps),
        false => println!("  max-steps          unset: a turn ends by its progress"),
    }
    println!("  context-tokens     {}", a.context_tokens);
    println!("  max-answer-tokens  {}", a.max_answer_tokens);
    println!("  transcripts        {}", a.transcripts);
    println!(
        "  sandbox            {}",
        match &a.sandbox {
            None => "off".to_string(),
            Some(s) if s.writable.is_empty() =>
                format!("{}, network {}", s.level.label(), s.network.label()),
            Some(s) => format!(
                "{}, network {}, writes in {}",
                s.level.label(),
                s.network.label(),
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
    // Names only: the values are never shown.
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
/// The subscriber never writes to stderr, because ratatui owns that screen.
fn init_logging(
    explicit: Option<PathBuf>,
) -> Result<Option<tracing_appender::non_blocking::WorkerGuard>> {
    use tracing_subscriber::EnvFilter;

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn,lokode=info"));

    let Some(path) = explicit.or_else(lokode::paths::log_file) else {
        // No home directory to write into: run without a log.
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
