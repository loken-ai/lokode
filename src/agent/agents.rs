//! Named agents a turn can hand work to.
//!
//! An agent is a prompt, the tools it may use, and the model it runs on. One is built in,
//! `explore`, which searches the project to answer a question. Others are Markdown files
//! in `~/.config/lokode/agents/` and in the project's `.lokode/agents/`, named by their
//! file, the project's last. A file opens with a block of settings between 2 `---`
//! lines, and the rest is the prompt.
//!
//! ```text
//! ---
//! description: Reviews the changes for bugs
//! tools: read_file, search, lsp_at
//! model: qwen3:4b
//! max-steps: 12
//! ---
//! Read the changes and say what is wrong with them.
//! ```

use std::path::{Path, PathBuf};

/// The agent a delegation runs when none is named.
pub const DEFAULT: &str = "explore";

/// One agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSpec {
    pub name: String,
    /// What it is for, as the model choosing it reads it.
    pub description: String,
    pub prompt: String,
    /// The tools it may use, by name. `None` is the tools that change nothing.
    pub tools: Option<Vec<String>>,
    /// The model it runs on, when it names one.
    pub model: Option<String>,
    /// The backend it runs on, by its name in `[backends]`, when it names one.
    pub backend: Option<String>,
    pub max_steps: Option<usize>,
    /// Where it was read from.
    pub from: String,
}

impl AgentSpec {
    /// The agent every session has.
    pub fn explore() -> Self {
        Self {
            name: DEFAULT.into(),
            description: "searches the project to answer one question, and changes nothing".into(),
            prompt: "You are answering one question for another agent. Use the tools to find \
                     out, then answer in a few lines. Nothing you read is returned, only what \
                     you say, so state the answer rather than pointing at where it is."
                .into(),
            tools: None,
            model: None,
            backend: None,
            max_steps: Some(8),
            from: "built in".into(),
        }
    }
}

/// The directories agents are read from: the user's, the plugins', then the project's.
pub fn dirs(root: &Path) -> Vec<PathBuf> {
    let user = crate::config::user_config_path().and_then(|p| p.parent().map(|d| d.join("agents")));
    user.into_iter()
        .chain(crate::plugins::dirs("agents"))
        .chain([root.join(".lokode").join("agents")])
        .collect()
}

/// The agents of a session: the built-in one, then those of `dirs` in order, a later one
/// taking the place of an earlier one of the same name. Says what it could not read.
pub fn load(dirs: &[PathBuf]) -> (Vec<AgentSpec>, Vec<String>) {
    let mut agents = vec![AgentSpec::explore()];
    let mut warnings = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "md"))
            .collect();
        paths.sort();
        for path in paths {
            let name = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            if !crate::frontmatter::is_name(&name) {
                warnings.push(format!(
                    "the agent {} is not named with letters, digits, - and _ only",
                    path.display()
                ));
                continue;
            }
            let parsed = std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .map(|text| crate::claude::in_plugin(&text, dir))
                .and_then(|text| parse(&name, &text, &path.display().to_string()));
            match parsed {
                Ok(spec) => {
                    agents.retain(|a| a.name != spec.name);
                    agents.push(spec);
                }
                Err(why) => warnings.push(format!("the agent {}: {why}", path.display())),
            }
        }
    }
    (agents, warnings)
}

/// Read an agent's file.
pub fn parse(name: &str, text: &str, from: &str) -> Result<AgentSpec, String> {
    let (settings, prompt) = crate::frontmatter::split(text)?;
    let mut spec = AgentSpec {
        name: name.into(),
        description: String::new(),
        prompt,
        tools: None,
        model: None,
        backend: None,
        max_steps: None,
        from: from.into(),
    };
    for (key, value) in settings {
        match key.as_str() {
            "description" => spec.description = value,
            // Named as lokode names its tools or as other tools do.
            "tools" => spec.tools = Some(crate::claude::tools_named(&value)),
            // Another tool's name for its own models names nothing lokode can run.
            "model" => {
                spec.model = Some(value).filter(|v| {
                    !v.is_empty()
                        && !crate::claude::MODEL_ALIASES.contains(&v.to_lowercase().as_str())
                })
            }
            "backend" => spec.backend = Some(value).filter(|v| !v.is_empty()),
            "max-steps" | "max-turns" | "maxTurns" => {
                spec.max_steps = Some(
                    value
                        .parse()
                        .map_err(|_| format!("max-steps must be a number, got {value:?}"))?,
                )
            }
            // Written by other tools' agents, and harmless here.
            "name" | "color" | "skills" | "permissionMode" | "isolation" | "memory" | "effort"
            | "disallowedTools" | "hooks" | "mcpServers" => {}
            other => return Err(format!("it has a setting {other:?} lokode does not know")),
        }
    }
    if spec.prompt.is_empty() {
        return Err("it has no prompt".into());
    }
    if spec.description.is_empty() {
        spec.description = spec.prompt.lines().next().unwrap_or_default().to_string();
    }
    Ok(spec)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The settings are read off the head of the file and the rest is the prompt.
    #[test]
    fn an_agent_is_read_from_its_file() {
        let spec = parse(
            "reviewer",
            "---\ndescription: Reviews the changes\ntools: read_file, search\nmodel: qwen3:4b\n\
             max-steps: 12\n---\nSay what is wrong.\n",
            "here",
        )
        .unwrap();
        assert_eq!(spec.description, "Reviews the changes");
        assert_eq!(
            spec.tools,
            Some(vec!["read_file".to_string(), "search".to_string()])
        );
        assert_eq!(spec.model.as_deref(), Some("qwen3:4b"));
        assert_eq!(spec.max_steps, Some(12));
        assert_eq!(spec.prompt, "Say what is wrong.");
    }

    /// A file without settings is all prompt, and its first line describes it.
    #[test]
    fn a_file_without_settings_is_a_prompt() {
        let spec = parse("t", "Write the tests.\nThen run them.", "here").unwrap();
        assert_eq!(spec.description, "Write the tests.");
        assert_eq!(spec.tools, None);
    }

    /// What cannot be understood is refused with a word.
    #[test]
    fn a_wrong_file_is_refused() {
        assert!(parse("t", "---\ncolour: red\n---\nx", "here").is_err());
        assert!(parse("t", "---\nmax-steps: many\n---\nx", "here").is_err());
        assert!(parse("t", "---\ndescription: x\n", "here").is_err());
        assert!(parse("t", "---\ndescription: x\n---\n", "here").is_err());
    }

    /// The project's agent takes the place of the user's of the same name, and the
    /// built-in one is always there.
    #[test]
    fn the_project_comes_last() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user");
        let project = dir.path().join("project");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(user.join("review.md"), "the user's").unwrap();
        std::fs::write(user.join("docs.md"), "write docs").unwrap();
        std::fs::write(project.join("review.md"), "the project's").unwrap();
        std::fs::write(project.join("bad name.md"), "x").unwrap();
        let (agents, warnings) = load(&[user, project]);
        let names: Vec<_> = agents.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["explore", "docs", "review"]);
        assert_eq!(agents[2].prompt, "the project's");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
    }
}
