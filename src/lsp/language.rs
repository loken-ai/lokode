//! Which language server answers for which file.

use std::path::Path;

use serde_json::Value;

/// One language server: what runs it and which files it answers for.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerSpec {
    /// The name the session knows the server by, one per language.
    pub language: String,
    pub command: String,
    pub args: Vec<String>,
    /// The file extensions it answers for, each with the language id the protocol names.
    pub extensions: Vec<(String, String)>,
    /// Sent as `initializationOptions`.
    pub init: Option<Value>,
    /// What `workspace/configuration` is answered with, by section.
    pub settings: Option<Value>,
    pub env: Vec<(String, String)>,
}

impl ServerSpec {
    fn builtin(language: &str, command: &str, args: &[&str], extensions: &[(&str, &str)]) -> Self {
        Self {
            language: language.to_string(),
            command: command.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            extensions: extensions
                .iter()
                .map(|(e, id)| (e.to_string(), id.to_string()))
                .collect(),
            init: None,
            settings: None,
            env: Vec::new(),
        }
    }

    /// The protocol's language id for `path`, when this server answers for it.
    pub fn language_id(&self, path: &Path) -> Option<&str> {
        let ext = path.extension()?.to_str()?;
        self.extensions
            .iter()
            .find(|(e, _)| e == ext)
            .map(|(_, id)| id.as_str())
    }

    /// This server with the command of `LOKODE_LSP_<LANGUAGE>` when it is set.
    fn overridden(mut self) -> Self {
        let var = format!("LOKODE_LSP_{}", self.language.to_uppercase());
        if let Ok(cmd) = std::env::var(&var) {
            let mut parts = cmd.split_whitespace().map(String::from);
            if let Some(program) = parts.next() {
                self.command = program;
                self.args = parts.collect();
            }
        }
        self
    }
}

/// The servers a session knows of.
#[derive(Debug, Clone, Default)]
pub struct Languages {
    pub specs: Vec<ServerSpec>,
}

impl Languages {
    /// The servers known without configuration, each overridable from the environment.
    pub fn builtin() -> Self {
        Self {
            specs: builtin_specs()
                .into_iter()
                .map(ServerSpec::overridden)
                .collect(),
        }
    }

    /// The built-in servers as the configuration changes them, then as the environment
    /// does.
    ///
    /// Says what it could not take from the configuration.
    pub fn from_config(section: &crate::config::LspSection) -> (Self, Vec<String>) {
        if section.enabled == Some(false) {
            return (Self::default(), Vec::new());
        }
        let mut specs = builtin_specs();
        let mut warnings = Vec::new();
        for (name, server) in &section.servers {
            let at = specs.iter().position(|s| &s.language == name);
            if server.enabled == Some(false) {
                if let Some(at) = at {
                    specs.remove(at);
                }
                continue;
            }
            let mut spec = match at {
                Some(at) => specs.remove(at),
                None => match (&server.command, &server.extensions) {
                    (Some(command), Some(_)) => ServerSpec::builtin(name, command, &[], &[]),
                    _ => {
                        warnings.push(format!(
                            "the language server {name} needs a command and extensions"
                        ));
                        continue;
                    }
                },
            };
            if let Some(command) = &server.command {
                spec.command = command.clone();
            }
            if let Some(args) = &server.args {
                spec.args = args.clone();
            }
            if let Some(extensions) = &server.extensions {
                spec.extensions = extensions
                    .iter()
                    .map(|e| {
                        let e = e.trim_start_matches('.').to_string();
                        let id = known_id(&e).unwrap_or(name).to_string();
                        (e, id)
                    })
                    .collect();
            }
            if server.init.is_some() {
                spec.init = server.init.clone();
            }
            if server.settings.is_some() {
                spec.settings = server.settings.clone();
            }
            spec.env
                .extend(server.env.iter().map(|(k, v)| (k.clone(), v.clone())));
            specs.push(spec);
        }
        let specs = specs.into_iter().map(ServerSpec::overridden).collect();
        (Self { specs }, warnings)
    }

    /// The server that answers for `path`.
    pub fn for_path(&self, path: &Path) -> Option<&ServerSpec> {
        self.specs.iter().find(|s| s.language_id(path).is_some())
    }

    pub fn named(&self, language: &str) -> Option<&ServerSpec> {
        self.specs.iter().find(|s| s.language == language)
    }
}

/// The language id the built-in servers give an extension.
fn known_id(ext: &str) -> Option<&'static str> {
    BUILTIN
        .iter()
        .flat_map(|(_, _, _, exts)| exts.iter())
        .find(|(e, _)| *e == ext)
        .map(|(_, id)| *id)
}

/// The built-in servers: language, command, arguments, and extensions with their ids.
type Builtin = (
    &'static str,
    &'static str,
    &'static [&'static str],
    &'static [(&'static str, &'static str)],
);

const BUILTIN: &[Builtin] = &[
    ("rust", "rust-analyzer", &[], &[("rs", "rust")]),
    (
        "python",
        "pyright-langserver",
        &["--stdio"],
        &[("py", "python"), ("pyi", "python")],
    ),
    (
        "typescript",
        "typescript-language-server",
        &["--stdio"],
        &[
            ("ts", "typescript"),
            ("tsx", "typescriptreact"),
            ("mts", "typescript"),
            ("cts", "typescript"),
            ("js", "javascript"),
            ("jsx", "javascriptreact"),
            ("mjs", "javascript"),
            ("cjs", "javascript"),
        ],
    ),
    ("go", "gopls", &[], &[("go", "go")]),
    (
        "c",
        "clangd",
        &[],
        &[
            ("c", "c"),
            ("h", "c"),
            ("cpp", "cpp"),
            ("cc", "cpp"),
            ("cxx", "cpp"),
            ("hpp", "cpp"),
            ("hh", "cpp"),
            ("hxx", "cpp"),
        ],
    ),
    ("zig", "zls", &[], &[("zig", "zig")]),
    (
        "bash",
        "bash-language-server",
        &["start"],
        &[("sh", "shellscript"), ("bash", "shellscript")],
    ),
    ("lua", "lua-language-server", &[], &[("lua", "lua")]),
    ("ruby", "ruby-lsp", &[], &[("rb", "ruby")]),
];

fn builtin_specs() -> Vec<ServerSpec> {
    BUILTIN
        .iter()
        .map(|(language, command, args, exts)| ServerSpec::builtin(language, command, args, exts))
        .collect()
}

/// Whether `command` names a program that can be run: a path to one, or a name found on
/// `PATH`.
pub fn installed(command: &str) -> bool {
    let runnable = |p: &Path| {
        use std::os::unix::fs::PermissionsExt;
        p.metadata()
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    };
    if command.contains('/') {
        return runnable(Path::new(command));
    }
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| runnable(&dir.join(command))))
        .unwrap_or(false)
}

/// How many entries of the project a look for its languages reads at most.
const LOOKED_AT: usize = 5000;

/// The languages of `languages` that have files under `root`, by how many files each has,
/// most first.
/// Reads at most `LOOKED_AT` entries.
/// Honours ignore files.
pub fn present(root: &Path, languages: &Languages) -> Vec<String> {
    let mut counts: std::collections::HashMap<&str, usize> = Default::default();
    for entry in ignore::WalkBuilder::new(root)
        .build()
        .take(LOOKED_AT)
        .flatten()
    {
        if let Some(spec) = languages.for_path(entry.path()) {
            *counts.entry(spec.language.as_str()).or_default() += 1;
        }
    }
    let mut found: Vec<_> = counts.into_iter().collect();
    found.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    found.into_iter().map(|(l, _)| l.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LspSection, LspServer};

    /// The configuration changes a built-in server, removes one, or adds one.
    /// An incomplete new one is refused with a word.
    #[test]
    fn the_configuration_changes_the_servers() {
        let mut section = LspSection::default();
        section.servers.insert(
            "rust".into(),
            LspServer {
                args: Some(vec!["--log".into()]),
                ..Default::default()
            },
        );
        section.servers.insert(
            "ruby".into(),
            LspServer {
                enabled: Some(false),
                ..Default::default()
            },
        );
        section.servers.insert(
            "elixir".into(),
            LspServer {
                command: Some("elixir-ls".into()),
                extensions: Some(vec![".ex".into(), "exs".into()]),
                ..Default::default()
            },
        );
        section.servers.insert(
            "nim".into(),
            LspServer {
                command: Some("nimlsp".into()),
                ..Default::default()
            },
        );
        let (languages, warnings) = Languages::from_config(&section);
        let rust = languages.named("rust").unwrap();
        assert_eq!(rust.args, ["--log"]);
        assert!(languages.named("ruby").is_none());
        let elixir = languages.named("elixir").unwrap();
        assert_eq!(elixir.language_id(Path::new("a.ex")), Some("elixir"));
        assert!(languages.named("nim").is_none());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let off = LspSection {
            enabled: Some(false),
            ..Default::default()
        };
        assert!(Languages::from_config(&off).0.specs.is_empty());
    }

    /// The languages of a project are those its files are written in, most first.
    /// A program is installed when it can be run.
    #[test]
    fn the_project_says_which_languages_it_uses() {
        let dir = tempfile::tempdir().unwrap();
        for f in ["a.rs", "b.rs", "c.py", "notes.md"] {
            std::fs::write(dir.path().join(f), "").unwrap();
        }
        let found = present(dir.path(), &Languages::builtin());
        assert_eq!(found, ["rust", "python"]);
        assert!(installed("sh"));
        assert!(!installed("surely-not-a-program-here"));
    }
}
