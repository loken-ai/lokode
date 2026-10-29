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
        let specs = vec![
            ServerSpec::builtin("rust", "rust-analyzer", &[], &[("rs", "rust")]),
            ServerSpec::builtin(
                "python",
                "pyright-langserver",
                &["--stdio"],
                &[("py", "python"), ("pyi", "python")],
            ),
            ServerSpec::builtin(
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
        ];
        Self {
            specs: specs.into_iter().map(ServerSpec::overridden).collect(),
        }
    }

    /// The server that answers for `path`.
    pub fn for_path(&self, path: &Path) -> Option<&ServerSpec> {
        self.specs.iter().find(|s| s.language_id(path).is_some())
    }
}
