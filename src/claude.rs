//! What lokode reads of the plugins other tools write, Claude Code's first.
//!
//! Such a plugin is a directory with the components in the places that tool gives them, and
//! a manifest in `.claude-plugin/plugin.json` that it may do without. lokode reads what it
//! has a place for and says what it leaves out.
//!
//! | In the plugin | lokode |
//! |---|---|
//! | `skills/`, `commands/`, `agents/` | loaded as they are, their settings it does not use ignored |
//! | `hooks/hooks.json` and the manifest's `hooks` | the events it has, with the tool names in lokode's |
//! | `.mcp.json` and the manifest's `mcpServers` | tool servers |
//! | `.claude-plugin/marketplace.json` | a marketplace, its sources a path, `github`, `url` or `git-subdir` |
//! | `${CLAUDE_PLUGIN_ROOT}` | the plugin's directory |
//!
//! A hook of such a plugin is given the fields that tool gives, beside lokode's own, so the
//! script it brings reads what it expects.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{json, Value};

use crate::config::{Hook, HooksSection, LspServer, McpServer};

/// The directory a Claude Code plugin keeps its manifest and its marketplace in.
pub const DIR: &str = ".claude-plugin";

/// The model names that tool uses for its own models. They name nothing lokode can run.
pub const MODEL_ALIASES: &[&str] = &["sonnet", "opus", "haiku", "fable", "inherit", "default"];

/// The tools of lokode a tool of Claude Code's name stands for.
pub fn tools_of(name: &str) -> Vec<&'static str> {
    match name {
        "Read" => vec!["read_file", "list_dir"],
        "Write" => vec!["write_file"],
        "Edit" | "MultiEdit" => vec!["edit_file", "edit_lines", "apply_patch"],
        "Bash" => vec!["bash"],
        "Grep" => vec!["grep"],
        "Glob" | "LS" => vec!["glob", "list_dir"],
        "WebFetch" => vec!["web_fetch"],
        "Task" | "Agent" => vec!["delegate"],
        "TodoWrite" => vec!["update_plan"],
        _ => Vec::new(),
    }
}

/// The name Claude Code gives a tool of lokode's.
pub fn name_of(tool: &str) -> String {
    match tool {
        "read_file" | "list_dir" => "Read".into(),
        "write_file" => "Write".into(),
        "edit_file" | "edit_lines" | "apply_patch" => "Edit".into(),
        "bash" | "run_tests" => "Bash".into(),
        "grep" => "Grep".into(),
        "glob" => "Glob".into(),
        "web_fetch" => "WebFetch".into(),
        "delegate" => "Task".into(),
        "update_plan" => "TodoWrite".into(),
        other => match other.split_once("__") {
            Some((server, name)) => format!("mcp__{server}__{name}"),
            None => other.to_string(),
        },
    }
}

/// A tool's arguments as Claude Code names them.
pub fn input_of(tool: &str, args: &Value) -> Value {
    let Some(map) = args.as_object() else {
        return args.clone();
    };
    let mut out = serde_json::Map::new();
    for (key, value) in map {
        let key = match (tool, key.as_str()) {
            (_, "path")
                if matches!(
                    tool,
                    "read_file" | "write_file" | "edit_file" | "edit_lines"
                ) =>
            {
                "file_path"
            }
            ("bash", "cmd") => "command",
            (_, other) => other,
        };
        out.insert(key.to_string(), value.clone());
    }
    Value::Object(out)
}

/// The tools a list of names stands for, each in lokode's name or Claude Code's. A name that
/// stands for nothing is left out.
pub fn tools_named(list: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for name in list.split(',').map(str::trim).filter(|n| !n.is_empty()) {
        let found: Vec<String> = match tools_of(name).as_slice() {
            [] => match name.strip_prefix("mcp__") {
                Some(rest) => vec![rest.to_string()],
                None if name.chars().next().is_some_and(|c| c.is_ascii_uppercase()) => Vec::new(),
                None => vec![name.to_string()],
            },
            tools => tools.iter().map(|t| t.to_string()).collect(),
        };
        for tool in found {
            if !out.contains(&tool) {
                out.push(tool);
            }
        }
    }
    out
}

/// A matcher of Claude Code's as the patterns lokode matches a tool by, or `None` when
/// nothing of it names a tool lokode has.
fn matcher(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() || text == "*" {
        return Some("*".into());
    }
    let mut out: Vec<String> = Vec::new();
    for part in text.split('|').map(str::trim) {
        let found: Vec<String> = match tools_of(part).as_slice() {
            [] => match part.strip_prefix("mcp__") {
                Some(rest) => vec![rest.replace(".*", "*")],
                None => Vec::new(),
            },
            tools => tools.iter().map(|t| t.to_string()).collect(),
        };
        for tool in found {
            if !out.contains(&tool) {
                out.push(tool);
            }
        }
    }
    (!out.is_empty()).then(|| out.join("|"))
}

/// `text`, a file of a plugin kept in `dir` (`commands`, `agents`, `skills`), with
/// `${CLAUDE_PLUGIN_ROOT}` put as the plugin's directory.
pub fn in_plugin(text: &str, dir: &Path) -> String {
    match (text.contains("${CLAUDE_PLUGIN_ROOT}"), dir.parent()) {
        (true, Some(root)) => with_root(text, root),
        _ => text.to_string(),
    }
}

/// What `text` says with `${CLAUDE_PLUGIN_ROOT}` put as `root`.
pub fn with_root(text: &str, root: &Path) -> String {
    text.replace("${CLAUDE_PLUGIN_ROOT}", &root.display().to_string())
}

/// The hooks a `hooks.json` holds, or an inline object of the manifest: the events lokode
/// has, a hook a command. Says what it leaves out.
pub fn hooks_from(value: &Value, root: &Path) -> (HooksSection, Vec<String>) {
    let mut out = HooksSection::default();
    let mut warnings = Vec::new();
    let events = value
        .get("hooks")
        .filter(|h| h.is_object())
        .unwrap_or(value);
    let Some(events) = events.as_object() else {
        return (out, warnings);
    };
    for (event, groups) in events {
        let slot = match event.as_str() {
            "SessionStart" => &mut out.session_start,
            "UserPromptSubmit" => &mut out.prompt,
            "PreToolUse" => &mut out.pre_tool,
            "PostToolUse" => &mut out.post_tool,
            "Stop" => &mut out.turn_end,
            "SubagentStop" => &mut out.agent_end,
            "PreCompact" => &mut out.compact,
            "SessionEnd" => &mut out.session_end,
            "Notification" => &mut out.notification,
            other => {
                warnings.push(format!("the hook event {other} is not one lokode has"));
                continue;
            }
        };
        let tool_event = matches!(event.as_str(), "PreToolUse" | "PostToolUse");
        for group in groups.as_array().into_iter().flatten() {
            let matches = match tool_event {
                true => {
                    match matcher(group.get("matcher").and_then(|m| m.as_str()).unwrap_or("")) {
                        Some(m) => Some(m),
                        None => {
                            warnings.push(format!(
                                "a {event} hook for {:?} names no tool lokode has",
                                group.get("matcher").and_then(|m| m.as_str()).unwrap_or("")
                            ));
                            continue;
                        }
                    }
                }
                false => None,
            };
            for hook in group
                .get("hooks")
                .and_then(|h| h.as_array())
                .into_iter()
                .flatten()
            {
                match (
                    hook.get("type").and_then(|t| t.as_str()),
                    hook.get("command").and_then(|c| c.as_str()),
                ) {
                    (Some("command") | None, Some(command)) => slot.push(Hook {
                        command: with_root(command, root),
                        matches: matches.clone(),
                        timeout: hook.get("timeout").and_then(|t| t.as_u64()),
                    }),
                    (other, _) => warnings.push(format!(
                        "a {event} hook of type {} is not a command, which is all lokode runs",
                        other.unwrap_or("?")
                    )),
                }
            }
        }
    }
    (out, warnings)
}

/// The tool servers a `.mcp.json` holds, or an inline object of the manifest, with the
/// servers either under `mcpServers` or named directly.
pub fn servers_from(value: &Value, root: &Path) -> (BTreeMap<String, McpServer>, Vec<String>) {
    let mut out = BTreeMap::new();
    let mut warnings = Vec::new();
    let named = value
        .get("mcpServers")
        .filter(|m| m.is_object())
        .unwrap_or(value);
    let Some(named) = named.as_object() else {
        return (out, warnings);
    };
    for (name, spec) in named {
        let text = |k: &str| {
            spec.get(k)
                .and_then(|v| v.as_str())
                .map(|v| with_root(v, root))
        };
        let strings = |k: &str| -> Vec<String> {
            spec.get(k)
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().map(|s| with_root(s, root)))
                .collect()
        };
        let pairs = |k: &str| -> Vec<(String, String)> {
            spec.get(k)
                .and_then(|v| v.as_object())
                .into_iter()
                .flatten()
                .filter_map(|(k, v)| Some((k.clone(), with_root(v.as_str()?, root))))
                .collect()
        };
        let (command, url) = (text("command"), text("url"));
        if command.is_none() && url.is_none() {
            warnings.push(format!("the server {name} has no command and no url"));
            continue;
        }
        out.insert(
            name.clone(),
            McpServer {
                command,
                args: strings("args"),
                url,
                headers: pairs("headers").into_iter().collect(),
                token_env: None,
                client_id: None,
                scopes: Vec::new(),
                env: pairs("env").into_iter().collect(),
                enabled: true,
                timeout: None,
            },
        );
    }
    (out, warnings)
}

/// The language servers a `.lsp.json` holds, or an inline object of the manifest.
pub fn lsp_from(value: &Value, root: &Path) -> (BTreeMap<String, LspServer>, Vec<String>) {
    let mut out = BTreeMap::new();
    let mut warnings = Vec::new();
    let Some(named) = value.as_object() else {
        return (out, warnings);
    };
    for (name, spec) in named {
        let Some(command) = spec.get("command").and_then(|c| c.as_str()) else {
            warnings.push(format!("the language server {name} has no command"));
            continue;
        };
        let extensions: Vec<String> = spec
            .get("extensionToLanguage")
            .and_then(|e| e.as_object())
            .map(|e| {
                e.keys()
                    .map(|k| k.trim_start_matches('.').to_string())
                    .collect()
            })
            .unwrap_or_default();
        if extensions.is_empty() {
            warnings.push(format!("the language server {name} names no extension"));
            continue;
        }
        out.insert(
            name.clone(),
            LspServer {
                command: Some(with_root(command, root)),
                args: spec.get("args").and_then(|a| a.as_array()).map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(|s| with_root(s, root)))
                        .collect()
                }),
                extensions: Some(extensions),
                init: spec.get("initializationOptions").cloned(),
                settings: spec.get("settings").cloned(),
                env: spec
                    .get("env")
                    .and_then(|e| e.as_object())
                    .into_iter()
                    .flatten()
                    .filter_map(|(k, v)| Some((k.clone(), with_root(v.as_str()?, root))))
                    .collect(),
                enabled: None,
            },
        );
    }
    (out, warnings)
}

/// What the manifest of a plugin says, and what the files beside it add.
pub struct Read {
    pub description: String,
    pub version: Option<String>,
    pub hooks: HooksSection,
    pub servers: BTreeMap<String, McpServer>,
    pub lsp: BTreeMap<String, LspServer>,
    pub warnings: Vec<String>,
}

/// Whether `dir` holds a plugin of Claude Code's layout: a manifest, or any of its
/// components in the place it gives them.
pub fn is_plugin(dir: &Path) -> bool {
    dir.join(DIR).join("plugin.json").is_file()
        || dir.join("hooks").join("hooks.json").is_file()
        || dir.join(".mcp.json").is_file()
        || dir.join(".lsp.json").is_file()
        || ["commands", "agents", "skills"]
            .iter()
            .any(|d| dir.join(d).is_dir())
}

/// `text` with each `${user_config.KEY}` put as the value of the option `KEY`.
fn with_options(text: &str, values: &BTreeMap<String, String>) -> String {
    let mut out = text.to_string();
    for (key, value) in values {
        out = out.replace(&format!("${{user_config.{key}}}"), value);
    }
    out
}

/// The values of the options a manifest asks for: what the user gave in `given`, or in the
/// variable `CLAUDE_PLUGIN_OPTION_<KEY>`, or the option's default. Says what is missing.
fn option_values(
    manifest: &Value,
    plugin: &str,
    given: &BTreeMap<String, String>,
) -> (BTreeMap<String, String>, Vec<String>) {
    let mut values = BTreeMap::new();
    let mut missing = Vec::new();
    let Some(asked) = manifest.get("userConfig").and_then(|u| u.as_object()) else {
        return (values, missing);
    };
    for (key, spec) in asked {
        let from_env = std::env::var(format!("CLAUDE_PLUGIN_OPTION_{}", key.to_uppercase())).ok();
        let default = spec.get("default").map(|d| match d {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        });
        match given.get(key).cloned().or(from_env).or(default) {
            Some(value) => {
                values.insert(key.clone(), value);
            }
            None if spec
                .get("required")
                .and_then(|r| r.as_bool())
                .unwrap_or(false) =>
            {
                missing.push(format!(
                    "it needs the option {key}: set it in [plugins.options.{plugin}] or in CLAUDE_PLUGIN_OPTION_{}",
                    key.to_uppercase()
                ));
            }
            None => {}
        }
    }
    (values, missing)
}

/// Read the plugin in `dir`, the options the user gave it in `given`.
pub fn read_plugin(dir: &Path, given: &BTreeMap<String, String>) -> Result<Read, String> {
    let manifest: Value = match std::fs::read_to_string(dir.join(DIR).join("plugin.json")) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| format!("plugin.json: {e}"))?,
        Err(_) => Value::Null,
    };
    let mut read = Read {
        description: manifest
            .get("description")
            .and_then(|d| d.as_str())
            .unwrap_or_default()
            .to_string(),
        version: manifest
            .get("version")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        hooks: HooksSection::default(),
        servers: BTreeMap::new(),
        lsp: BTreeMap::new(),
        warnings: Vec::new(),
    };
    // A file the manifest names, or the object itself, or the default place.
    let sources = |key: &str, default: &str| -> Vec<Result<Value, String>> {
        let parse = |at: &Path, text: String| {
            serde_json::from_str::<Value>(&text).map_err(|e| format!("{}: {e}", at.display()))
        };
        let mut out = Vec::new();
        if let Ok(text) = std::fs::read_to_string(dir.join(default)) {
            out.push(parse(&dir.join(default), text));
        }
        let named = match manifest.get(key) {
            Some(Value::Array(list)) => list.clone(),
            Some(other) => vec![other.clone()],
            None => Vec::new(),
        };
        for item in named {
            match item {
                Value::String(path) => {
                    let at = dir.join(path.trim_start_matches("./"));
                    match std::fs::read_to_string(&at) {
                        Ok(text) => out.push(parse(&at, text)),
                        Err(e) => out.push(Err(format!("{}: {e}", at.display()))),
                    }
                }
                object => out.push(Ok(object)),
            }
        }
        out
    };
    for value in sources("hooks", "hooks/hooks.json") {
        match value {
            Ok(value) => {
                let (hooks, warnings) = hooks_from(&value, dir);
                read.hooks = std::mem::take(&mut read.hooks).after(hooks);
                read.warnings.extend(warnings);
            }
            Err(why) => read.warnings.push(why),
        }
    }
    let plugin = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let (values, missing) = option_values(&manifest, &plugin, given);
    read.warnings.extend(missing);
    for value in sources("lspServers", ".lsp.json") {
        match value {
            Ok(value) => {
                let (servers, warnings) = lsp_from(&value, dir);
                read.lsp.extend(servers);
                read.warnings.extend(warnings);
            }
            Err(why) => read.warnings.push(why),
        }
    }
    for value in sources("mcpServers", ".mcp.json") {
        match value {
            Ok(value) => {
                let (servers, warnings) = servers_from(&value, dir);
                // A server named later takes the place of an earlier one.
                read.servers.extend(servers);
                read.warnings.extend(warnings);
            }
            Err(why) => read.warnings.push(why),
        }
    }
    // The options go in what runs: a token in a server's environment or its headers.
    for server in read.servers.values_mut() {
        server.command = server.command.take().map(|c| with_options(&c, &values));
        server.url = server.url.take().map(|u| with_options(&u, &values));
        for arg in &mut server.args {
            *arg = with_options(arg, &values);
        }
        for value in server.env.values_mut() {
            *value = with_options(value, &values);
        }
        for value in server.headers.values_mut() {
            *value = with_options(value, &values);
        }
    }
    for server in read.lsp.values_mut() {
        server.command = server.command.take().map(|c| with_options(&c, &values));
        for arg in server.args.iter_mut().flatten() {
            *arg = with_options(arg, &values);
        }
        for value in server.env.values_mut() {
            *value = with_options(value, &values);
        }
    }
    Ok(read)
}

/// Where a plugin a marketplace lists comes from, in the forms Claude Code writes.
pub fn source_of(entry: &Value) -> Result<crate::marketplace::Source, String> {
    use crate::marketplace::Source;
    let text = |v: &Value, k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    match entry.get("source") {
        Some(Value::String(path)) => Ok(Source::Dir(path.trim_start_matches("./").into())),
        Some(object) => match text(object, "source").as_deref() {
            Some("github") => {
                let repo = text(object, "repo").ok_or("a github source has no repo")?;
                Ok(Source::Git(format!("https://github.com/{repo}.git")))
            }
            Some("url") => Ok(Source::Git(
                text(object, "url").ok_or("a url source has no url")?,
            )),
            Some("git-subdir") => {
                let url = text(object, "url").ok_or("a git-subdir source has no url")?;
                let url = match url.contains("://") || url.starts_with("git@") {
                    true => url,
                    false => format!("https://github.com/{url}.git"),
                };
                Ok(Source::Subdir {
                    url,
                    path: text(object, "path").ok_or("a git-subdir source has no path")?,
                })
            }
            Some("archive") => Ok(Source::Archive(
                text(object, "url").ok_or("an archive source has no url")?,
            )),
            Some("npm") => Ok(Source::Npm {
                package: text(object, "package").ok_or("an npm source has no package")?,
                version: text(object, "version"),
            }),
            other => Err(format!(
                "a source of kind {other:?} is not one lokode reads"
            )),
        },
        None => Err("it has no source".into()),
    }
}

/// One plugin a `marketplace.json` lists.
pub struct Listed {
    pub name: String,
    pub description: String,
    pub version: Option<String>,
    /// Where it comes from, or why lokode cannot read that.
    pub source: Result<crate::marketplace::Source, String>,
}

/// A `marketplace.json` as its description and the plugins it lists.
pub fn marketplace_from(text: &str) -> Result<(String, Vec<Listed>), String> {
    let value: Value = serde_json::from_str(text).map_err(|e| format!("marketplace.json: {e}"))?;
    let description = value
        .get("description")
        .and_then(|d| d.as_str())
        .unwrap_or_default()
        .to_string();
    let plugins = value
        .get("plugins")
        .and_then(|p| p.as_array())
        .ok_or("marketplace.json lists no plugins")?;
    let mut out = Vec::new();
    for entry in plugins {
        let Some(name) = entry.get("name").and_then(|n| n.as_str()) else {
            continue;
        };
        out.push(Listed {
            name: name.to_string(),
            description: entry
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or_default()
                .to_string(),
            version: entry
                .get("version")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            source: source_of(entry),
        });
    }
    Ok((description, out))
}

/// What a hook of a Claude Code plugin is given beside lokode's own fields: the event and
/// the tool as that tool names them.
pub fn hook_fields(event: &str, facts: &Value) -> Value {
    let mut out = json!({
        "hook_event_name": match event {
            "session-start" => "SessionStart",
            "prompt" => "UserPromptSubmit",
            "pre-tool" => "PreToolUse",
            "post-tool" => "PostToolUse",
            "turn-end" => "Stop",
            "agent-end" => "SubagentStop",
            "compact" => "PreCompact",
            "session-end" => "SessionEnd",
            "notification" => "Notification",
            other => other,
        },
    });
    if let Some(tool) = facts.get("tool").and_then(|t| t.as_str()) {
        out["tool_name"] = json!(name_of(tool));
        if let Some(args) = facts.get("args") {
            out["tool_input"] = input_of(tool, args);
        }
        if let Some(result) = facts.get("result") {
            out["tool_response"] = result.clone();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hooks come over with lokode's tools in the matcher, the plugin's root in the
    /// command, and what lokode has no event for said.
    #[test]
    fn hooks_are_read_in_lokodes_names() {
        let (hooks, warnings) = hooks_from(
            &json!({"hooks": {
                "PostToolUse": [{"matcher": "Write|Edit", "hooks": [
                    {"type": "command", "command": "\"${CLAUDE_PLUGIN_ROOT}\"/fmt.sh", "timeout": 10}]}],
                "PreToolUse": [{"hooks": [{"type": "command", "command": "guard"}]}],
                "Stop": [{"hooks": [{"type": "command", "command": "done"}]}],
                "Elicitation": [{"hooks": [{"type": "command", "command": "x"}]}],
                "SessionEnd": [{"hooks": [{"type": "command", "command": "bye"}]}]
            }}),
            Path::new("/p/plug"),
        );
        assert_eq!(hooks.post_tool[0].command, "\"/p/plug\"/fmt.sh");
        assert_eq!(
            hooks.post_tool[0].matches.as_deref(),
            Some("write_file|edit_file|edit_lines|apply_patch")
        );
        assert_eq!(hooks.post_tool[0].timeout, Some(10));
        assert_eq!(hooks.pre_tool[0].matches.as_deref(), Some("*"));
        assert_eq!(hooks.turn_end.len(), 1);
        assert_eq!(hooks.session_end.len(), 1);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
    }

    /// A hook is given its tool as Claude Code names it, with the arguments to match.
    #[test]
    fn a_hook_is_given_the_fields_it_expects() {
        let f = hook_fields("pre-tool", &json!({"tool": "bash", "args": {"cmd": "ls"}}));
        assert_eq!(f["hook_event_name"], "PreToolUse");
        assert_eq!(f["tool_name"], "Bash");
        assert_eq!(f["tool_input"]["command"], "ls");
        let g = hook_fields(
            "post-tool",
            &json!({"tool": "write_file", "args": {"path": "a", "content": "x"}}),
        );
        assert_eq!(g["tool_input"]["file_path"], "a");
        assert_eq!(name_of("lookup__word"), "mcp__lookup__word");
    }

    /// Servers come in both shapes, with the root in what runs, and tool names of both
    /// tools are read.
    #[test]
    fn servers_and_tools_are_read() {
        let (servers, _) = servers_from(
            &json!({"mcpServers": {
                "local": {"command": "${CLAUDE_PLUGIN_ROOT}/srv", "args": ["--x"], "env": {"K": "v"}},
                "remote": {"type": "http", "url": "https://x/mcp", "headers": {"A": "b"}},
                "broken": {}
            }}),
            Path::new("/p"),
        );
        assert_eq!(servers.len(), 2);
        assert_eq!(servers["local"].command.as_deref(), Some("/p/srv"));
        assert_eq!(servers["remote"].url.as_deref(), Some("https://x/mcp"));
        assert_eq!(
            tools_named("Read, Grep, Bash, mcp__tracker__find, read_file, WebSearch"),
            ["read_file", "list_dir", "grep", "bash", "tracker__find"]
        );
    }

    /// A language server comes over with its extensions without the dot.
    #[test]
    fn language_servers_are_read() {
        let (servers, warnings) = lsp_from(
            &json!({
                "go": {"command": "gopls", "args": ["serve"], "extensionToLanguage": {".go": "go"}},
                "bare": {"command": "x"}
            }),
            Path::new("/p"),
        );
        assert_eq!(
            servers["go"].extensions.as_deref(),
            Some(&["go".to_string()][..])
        );
        assert_eq!(
            servers["go"].args.as_deref(),
            Some(&["serve".to_string()][..])
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
    }

    /// A hooks file that is not JSON is said, not skipped.
    #[test]
    fn an_unreadable_file_is_said() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("hooks")).unwrap();
        std::fs::write(dir.path().join("hooks/hooks.json"), "{not json").unwrap();
        let read = read_plugin(dir.path(), &BTreeMap::new()).unwrap();
        assert_eq!(read.warnings.len(), 1, "{:?}", read.warnings);
        assert!(read.hooks.post_tool.is_empty());
    }

    /// An option the user gave, or the manifest defaults, goes in what runs, and one that
    /// is required and missing is said.
    #[test]
    fn options_go_in_what_runs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(DIR)).unwrap();
        std::fs::write(
            dir.path().join(DIR).join("plugin.json"),
            r#"{"name": "t", "userConfig": {
                "token": {"type": "string", "title": "t", "description": "d", "required": true},
                "region": {"type": "string", "title": "r", "description": "d", "default": "eu"}},
               "mcpServers": {"api": {"url": "https://x/${user_config.region}",
                   "headers": {"Authorization": "Bearer ${user_config.token}"}}}}"#,
        )
        .unwrap();
        let none = read_plugin(dir.path(), &BTreeMap::new()).unwrap();
        assert!(
            none.warnings
                .iter()
                .any(|w| w.contains("needs the option token")),
            "{:?}",
            none.warnings
        );
        assert_eq!(none.servers["api"].url.as_deref(), Some("https://x/eu"));
        let given = BTreeMap::from([("token".to_string(), "s3cret".to_string())]);
        let with = read_plugin(dir.path(), &given).unwrap();
        assert!(with.warnings.is_empty(), "{:?}", with.warnings);
        assert_eq!(
            with.servers["api"].headers["Authorization"],
            "Bearer s3cret"
        );
    }

    /// The 4 forms of a source are read.
    #[test]
    fn a_marketplace_names_its_sources() {
        let (about, plugins) = marketplace_from(
            r#"{"name": "m", "description": "ours", "plugins": [
                {"name": "a", "source": "./plugins/a"},
                {"name": "b", "source": {"source": "github", "repo": "o/b"}},
                {"name": "c", "source": {"source": "url", "url": "https://h/c.git"}},
                {"name": "d", "source": {"source": "git-subdir", "url": "o/mono", "path": "tools/d"}},
                {"name": "e", "source": {"source": "npm", "package": "e", "version": "1.2.0"}},
                {"name": "f", "source": {"source": "archive", "url": "https://h/f.zip"}},
                {"name": "g", "source": {"source": "command", "command": "make"}}
            ]}"#,
        )
        .unwrap();
        assert_eq!(about, "ours");
        use crate::marketplace::Source;
        assert_eq!(plugins[0].source, Ok(Source::Dir("plugins/a".into())));
        assert_eq!(
            plugins[1].source,
            Ok(Source::Git("https://github.com/o/b.git".into()))
        );
        assert_eq!(plugins[2].source, Ok(Source::Git("https://h/c.git".into())));
        assert_eq!(
            plugins[3].source,
            Ok(Source::Subdir {
                url: "https://github.com/o/mono.git".into(),
                path: "tools/d".into()
            })
        );
        assert_eq!(
            plugins[4].source,
            Ok(Source::Npm {
                package: "e".into(),
                version: Some("1.2.0".into())
            })
        );
        assert_eq!(
            plugins[5].source,
            Ok(Source::Archive("https://h/f.zip".into()))
        );
        assert!(plugins[6].source.is_err());
    }
}
