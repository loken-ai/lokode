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

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct FileConfig {
    pub model: Option<String>,
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
        tool_protocol: over.tool_protocol.or(base.tool_protocol),
        backend: BackendSection {
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
