//! Settings, and the four places they can come from.
//!
//! Every knob used to be a command-line flag and nothing else, so the model had to be
//! retyped at every launch and a project could not say anything about how it wanted to be
//! worked on. Files answer both, and the precedence is the ordinary one: a flag beats the
//! environment, the environment beats the project, the project beats the user, and the
//! user beats the built-in default. The narrower statement wins, because it is the one
//! made with more knowledge of the situation.

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
    Some(base.join("verve").join("config.toml"))
}

/// The file a project keeps for everyone who works on it.
pub fn project_config_path(root: &Path) -> PathBuf {
    root.join(".verve.toml")
}

/// The project's standing instructions to the agent: its conventions, its build, what it
/// must not touch. Committed, so the whole team works with the same ones.
pub fn project_instructions_path(root: &Path) -> PathBuf {
    root.join(".verve.md")
}

/// The user's own standing instructions, for every project.
pub fn user_instructions_path() -> Option<PathBuf> {
    user_config_path().map(|p| p.with_file_name("verve.md"))
}

/// The standing instructions, user's first and project's second, and anything the user
/// should be told about reading them.
///
/// They are appended to the system message, which compaction never drops, so they hold
/// the window for the whole session and are bounded. Trimming is said out loud: text the
/// agent was told to follow and did not receive would otherwise look like text it ignored.
pub fn load_instructions(root: &Path, max_chars: usize) -> (Option<String>, Vec<String>) {
    let files = [
        user_instructions_path(),
        Some(project_instructions_path(root)),
    ];
    instructions_from(files.into_iter().flatten(), max_chars)
}

/// [`load_instructions`] against paths a caller names, which is what lets a test drive it
/// without moving the process's home directory.
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
    root.join(".verve").join("commands")
}

/// The text of a command the project defines, if it defines one by that name.
///
/// A command is a file of instructions, not a program: what it holds is handed to the
/// model as the turn's prompt. That is how a team ships a way of working - a review
/// checklist, a release routine - without anyone writing Rust.
pub fn user_command(root: &Path, name: &str) -> Option<String> {
    // A name that walks out of the directory is not a command name.
    if name.is_empty() || name.contains(['/', '\\', '.']) {
        return None;
    }
    let path = commands_dir(root).join(format!("{name}.md"));
    std::fs::read_to_string(path)
        .ok()
        .map(|t| t.trim().to_string())
}

/// The commands a project defines, by name.
pub fn user_commands(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(commands_dir(root))
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    name.strip_suffix(".md").map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// A first draft of a project's instructions, from what the repository shows.
///
/// Written rather than guessed at by the model: a file the user edits is a file they
/// trust, and the layout and the test command are things the repository already states.
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
    /// What a particular model needs, by name. Only the window differs in practice, and
    /// it differs enough to matter: a budget inherited from another model is a budget
    /// that either wastes the window or overruns it.
    #[serde(default)]
    pub models: BTreeMap<String, ModelSection>,
    /// What may go ahead without a question, and what may never go ahead at all.
    #[serde(default)]
    pub approve: crate::sandbox::policy::Policy,
    /// Commands to run at the edges of a tool call and a turn.
    #[serde(default)]
    pub hooks: HooksSection,
    /// Which model does which kind of work.
    #[serde(default)]
    pub roles: crate::agent::roles::Roles,
    /// Whether to have a nearby backend hold the next model ready while other work runs.
    /// On by default, and a no-op against anything that is not nearby.
    pub preload: Option<bool>,
    /// Whether to look into the project while nobody is waiting, and remember what is
    /// found. Off by default: it spends the machine's time on its own.
    pub discover: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct HooksSection {
    /// Run before a tool that changes something. A non-zero exit refuses the call and
    /// what it printed is handed to the model, which is what makes a policy check part
    /// of the loop rather than advice nobody reads.
    pub pre_tool: Option<String>,
    /// Run after such a tool, whatever it did.
    pub post_tool: Option<String>,
    /// Run when a turn ends. A formatter or a test suite belongs here.
    pub turn_end: Option<String>,
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
    /// Make everything outside the project read-only for an approved command.
    ///
    /// Off by default: it can make a build fail in a way that looks like a bug in the
    /// build, and breaking someone's work to add a protection they did not ask for is the
    /// wrong trade. On, it is the difference between `rm -rf ~` working and not.
    pub confine_writes: Option<bool>,
    /// Directories a confined command may still write to, on top of the project.
    ///
    /// A build wants its caches - `~/.cargo`, `~/.npm`, a temporary directory - and without
    /// them confinement is strict enough that nobody leaves it on.
    #[serde(default)]
    pub writable: Vec<PathBuf>,
}

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
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    /// Set false to keep a server configured but not started.
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct BackendSection {
    /// Which API the backend speaks.
    ///
    /// `auto` reads it off the URL, which is right for every case anyone actually has:
    /// api.anthropic.com is Anthropic, everything else is OpenAI-compatible. Name one
    /// explicitly to reach an Anthropic-compatible proxy on your own host.
    pub kind: Option<BackendKind>,
    pub url: Option<String>,
    /// A key written in the file itself.
    ///
    /// Refused when the file is readable by anyone else, the way ssh refuses a private
    /// key with loose permissions - a secret in a world-readable file is not a secret,
    /// and failing quietly here would be the worst of both.
    pub api_key: Option<String>,
    /// The name of an environment variable to take the key from instead. Preferred, since
    /// it keeps the secret out of a file that tends to end up in version control.
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
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let cfg: FileConfig =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;

    if cfg.backend.api_key.is_some() && is_readable_by_others(path) {
        anyhow::bail!(
            "{} contains an api-key and is readable by others; \
             run `chmod 600 {}`, or use api-key-env to name a variable instead",
            path.display(),
            path.display()
        );
    }
    Ok(Some(cfg))
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
    FileConfig {
        model: over.model.or(base.model),
        mode: over.mode.or(base.mode),
        tool_protocol: over.tool_protocol.or(base.tool_protocol),
        backend: BackendSection {
            kind: over.backend.kind.or(base.backend.kind),
            url: over.backend.url.or(base.backend.url),
            api_key: over.backend.api_key.or(base.backend.api_key),
            api_key_env: over.backend.api_key_env.or(base.backend.api_key_env),
            headers: {
                let mut h = base.backend.headers;
                h.extend(over.backend.headers);
                h
            },
        },
        limits: LimitsSection {
            max_steps: over.limits.max_steps.or(base.limits.max_steps),
            context_tokens: over.limits.context_tokens.or(base.limits.context_tokens),
            max_answer_tokens: over
                .limits
                .max_answer_tokens
                .or(base.limits.max_answer_tokens),
        },
        web: WebSection {
            allow_hosts: {
                // Added to rather than replaced: a project naming its own dev server
                // should not silently drop the ones the user allowed everywhere.
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
            confine_writes: over.shell.confine_writes.or(base.shell.confine_writes),
            writable: {
                let mut w = base.shell.writable;
                w.extend(over.shell.writable);
                w
            },
        },
        roles: crate::agent::roles::Roles::merge(base.roles, over.roles),
        preload: over.preload.or(base.preload),
        discover: over.discover.or(base.discover),
        hooks: HooksSection {
            pre_tool: over.hooks.pre_tool.or(base.hooks.pre_tool),
            post_tool: over.hooks.post_tool.or(base.hooks.post_tool),
            turn_end: over.hooks.turn_end.or(base.hooks.turn_end),
        },
        approve: crate::sandbox::policy::Policy {
            // Added to rather than replaced, on both sides: a project stating what is
            // safe in it must not drop the user's own refusals, and a user's refusal must
            // survive a project that says nothing about it.
            allow: merge_rules(base.approve.allow, over.approve.allow),
            deny: merge_rules(base.approve.deny, over.approve.deny),
            max_auto: over.approve.max_auto.min(base.approve.max_auto),
        },
        models: {
            let mut models = base.models;
            models.extend(over.models);
            models
        },
        mcp: McpSection {
            servers: {
                // Merged by name so a project can add a server without restating the
                // user's, and can replace one of theirs by using the same name.
                let mut servers = base.mcp.servers;
                servers.extend(over.mcp.servers);
                servers
            },
        },
    }
}

/// The user's file, then the project's.
pub fn load(root: &Path) -> Result<FileConfig> {
    let mut cfg = FileConfig::default();
    if let Some(path) = user_config_path() {
        if let Some(user) = read(&path)? {
            cfg = merge(cfg, user);
        }
    }
    if let Some(project) = read(&project_config_path(root))? {
        cfg = merge(cfg, project);
    }
    Ok(cfg)
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
/// The rest of the file is left as it is, comments included: a tool that rewrites a
/// file a person edits has to hand it back recognisable. Only a top-level `model =` line
/// is touched, and one is added at the top when there is none.
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
        // Past the first table header a `model =` belongs to that table, not to the
        // top level, and rewriting it would change a different setting entirely.
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
/// The key is looked for in the environment first, because that is where a secret belongs
/// and it lets a shared project file name a variable without holding its value.
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

    let key = std::env::var("VERVE_API_KEY")
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
        // Sent in the clear otherwise, to a machine that is not this one.
        if sends_secret_in_clear(backend_url) {
            warnings.push(format!(
                "sending an API key over plain http to {backend_url}; anyone on the path can read it"
            ));
        }
        headers.push(("Authorization".into(), format!("Bearer {key}")));
    }
    Ok(Headers { headers, warnings })
}

/// Whether a credential sent to this URL would cross a network unencrypted.
/// Whether the backend's weights are near enough that loading them is worth arranging.
///
/// Warming is about a server that holds gigabytes on a card beside you: the load is
/// seconds and hiding it behind other work is time gained for nothing. A hosted endpoint
/// has nothing to warm and would be charged for the attempt, so it is left alone. Read off
/// the address rather than declared, since nobody would remember to declare it.
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

    /// A key in a file anyone can read is not a key. Failing loudly beats using it.
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

    /// A file with no key in it is nobody's business how it is chmodded.
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

    /// A typo in a config file should be reported, not silently ignored.
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

    /// Sending a bearer token in the clear to another machine is worth saying out loud;
    /// sending one to this machine is the ordinary local case and is not.
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

    /// A project's instructions come after the user's, and each is named so the model can
    /// tell whose rule it is reading.
    #[test]
    fn both_files_are_read_and_the_project_comes_last() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".config").join("verve")).unwrap();
        std::fs::write(
            home.join(".config").join("verve").join("verve.md"),
            "always answer in French",
        )
        .unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(project_instructions_path(&root), "run cargo test").unwrap();

        let (text, warnings) = instructions_from(
            [
                home.join(".config").join("verve").join("verve.md"),
                project_instructions_path(&root),
            ],
            10_000,
        );
        let text = text.unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        let user_at = text.find("always answer in French").unwrap();
        let project_at = text.find("run cargo test").unwrap();
        assert!(user_at < project_at);
        assert!(text.contains("verve.md"));
    }

    #[test]
    fn nothing_written_is_nothing_sent() {
        let dir = tempfile::tempdir().unwrap();
        let (text, warnings) = load_instructions(dir.path(), 10_000);
        assert!(text.is_none());
        assert!(warnings.is_empty());
    }

    /// Text the agent was told to follow and did not receive would read as text it
    /// ignored, so a trim is announced.
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

    /// The file and the directory are made when they are not there, so remembering works
    /// on a machine that has never had a configuration.
    #[test]
    fn remembering_creates_what_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("verve").join("config.toml");
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
mod command_tests {
    use super::*;

    fn with_command(dir: &Path, name: &str, body: &str) {
        let d = commands_dir(dir);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(format!("{name}.md")), body).unwrap();
    }

    #[test]
    fn a_project_command_is_read_and_listed() {
        let dir = tempfile::tempdir().unwrap();
        with_command(
            dir.path(),
            "review",
            "Review the diff against our conventions.",
        );
        with_command(dir.path(), "release", "Cut a release.");
        assert_eq!(
            user_command(dir.path(), "review").as_deref(),
            Some("Review the diff against our conventions.")
        );
        assert_eq!(user_command(dir.path(), "nope"), None);
        assert_eq!(user_commands(dir.path()), vec!["release", "review"]);
    }

    /// A name is a name, not a path: nothing outside the directory is a command.
    #[test]
    fn a_name_that_walks_out_is_not_a_command() {
        let dir = tempfile::tempdir().unwrap();
        with_command(dir.path(), "ok", "fine");
        std::fs::write(dir.path().join("secret.md"), "not a command").unwrap();
        assert_eq!(user_command(dir.path(), "../secret"), None);
        assert_eq!(user_command(dir.path(), "..\\secret"), None);
        assert_eq!(user_command(dir.path(), "secret.md"), None);
        assert_eq!(user_command(dir.path(), ""), None);
    }
}

#[cfg(test)]
mod nearby_tests {
    use super::*;

    /// Warming is about a server whose weights sit on a card beside you. A hosted
    /// endpoint has nothing to warm and would be billed for the attempt.
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
