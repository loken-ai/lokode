//! Commands written as Markdown files.
//!
//! A command is a prompt a person runs by name: `/review`, `/git:commit`.
//! It lives in `~/.config/lokode/commands/` for every project or in the project's
//! `.lokode/commands/`.
//! The project's command takes the place of the user's of the same name.
//! A file in a directory is named after both, joined by `:`.
//! A file may open with settings between 2 `---` lines:
//!
//! ```text
//! ---
//! description: commit what is staged, with a message in the project's style
//! argument-hint: [scope]
//! agent: scribe
//! ---
//! Write a commit message for what is staged, scoped to $1.
//! ```
//!
//! What is typed after the name replaces `$ARGUMENTS`.
//! Its words replace `$1` to `$9`.
//! A command that names none of them has what was typed appended.

use std::path::{Path, PathBuf};

/// One command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub name: String,
    /// What it does, as the list of commands shows it.
    pub description: String,
    /// What it takes, as the list shows it.
    pub argument_hint: Option<String>,
    /// The agent its prompt is handed to, when it names one.
    pub agent: Option<String>,
    pub body: String,
    /// Where it was read from.
    pub from: String,
}

impl Command {
    /// The prompt it makes with `args`, what was typed after its name.
    pub fn prompt(&self, args: &str) -> String {
        let text = expand(&self.body, args);
        match &self.agent {
            Some(agent) => format!("@{agent} {text}"),
            None => text,
        }
    }
}

/// The directories commands are read from: the user's, the plugins', then the project's.
pub fn dirs(root: &Path) -> Vec<PathBuf> {
    let user =
        crate::config::user_config_path().and_then(|p| p.parent().map(|d| d.join("commands")));
    user.into_iter()
        .chain(crate::plugins::dirs("commands"))
        .chain([crate::config::commands_dir(root)])
        .collect()
}

/// The commands of `dirs`, by name.
/// A later directory's command takes the place of an earlier one's.
/// Says what it could not read.
pub fn load(dirs: &[PathBuf]) -> (Vec<Command>, Vec<String>) {
    let mut out: Vec<Command> = Vec::new();
    let mut warnings = Vec::new();
    for dir in dirs {
        for (name, path) in files(dir) {
            let read = std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .map(|text| crate::claude::in_plugin(&text, dir))
                .and_then(|text| parse(&name, &text, &path.display().to_string()));
            match read {
                Ok(c) => {
                    out.retain(|o| o.name != c.name);
                    out.push(c);
                }
                Err(why) => warnings.push(format!("the command {}: {why}", path.display())),
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    (out, warnings)
}

/// The command files of `dir` with their names.
/// A file of `dir` is named by its own name.
/// A file of a directory in it is named by both names joined by `:`.
/// A name that is not made of letters, digits, `-` and `_` is left out.
fn files(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        if path.is_dir() && crate::frontmatter::is_name(&stem) {
            for (name, inner) in files(&path) {
                if !name.contains(':') {
                    out.push((format!("{stem}:{name}"), inner));
                }
            }
        } else if path.extension().is_some_and(|e| e == "md") && crate::frontmatter::is_name(&stem)
        {
            out.push((stem, path));
        }
    }
    out
}

/// Read a command's file.
pub fn parse(name: &str, text: &str, from: &str) -> Result<Command, String> {
    let (settings, body) = crate::frontmatter::split(text)?;
    let mut command = Command {
        name: name.into(),
        description: String::new(),
        argument_hint: None,
        agent: None,
        body,
        from: from.into(),
    };
    for (key, value) in settings {
        match key.as_str() {
            "description" => command.description = value,
            "argument-hint" => command.argument_hint = Some(value).filter(|v| !v.is_empty()),
            "agent" => command.agent = Some(value).filter(|v| !v.is_empty()),
            // Written by other tools' commands, and ignored here.
            "allowed-tools" | "model" | "disable-model-invocation" | "hooks" | "name" | "color" => {
            }
            other => return Err(format!("it has a setting {other:?} lokode does not know")),
        }
    }
    if command.body.is_empty() {
        return Err("it has no prompt".into());
    }
    if command.description.is_empty() {
        command.description = command.body.lines().next().unwrap_or_default().to_string();
    }
    Ok(command)
}

/// `body` with what was typed put in.
/// All of it goes for `$ARGUMENTS` and `$ARGS`.
/// Its words go for `$1` to `$9`, and a word in double quotes keeps its spaces.
/// It is appended when the body names none of them.
pub fn expand(body: &str, args: &str) -> String {
    let args = args.trim();
    let words = words(args);
    let mut named = false;
    let mut out = String::new();
    let mut rest = body;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        if let Some(tail) = after.strip_prefix("ARGUMENTS") {
            out.push_str(args);
            rest = tail;
            named = true;
        } else if let Some(tail) = after.strip_prefix("ARGS") {
            out.push_str(args);
            rest = tail;
            named = true;
        } else if let Some(d) = after.chars().next().filter(|c| ('1'..='9').contains(c)) {
            let n = d.to_digit(10).unwrap_or(1) as usize;
            out.push_str(words.get(n - 1).map(String::as_str).unwrap_or(""));
            rest = &after[1..];
            named = true;
        } else {
            out.push('$');
            rest = after;
        }
    }
    out.push_str(rest);
    match (named, args.is_empty()) {
        (false, false) => format!("{out}\n\n{args}"),
        _ => out,
    }
}

/// The words of `text`, a run in double quotes counting as one.
fn words(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    for c in text.chars() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !word.is_empty() {
                    out.push(std::mem::take(&mut word));
                }
            }
            c => word.push(c),
        }
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What was typed goes where the body says, whole or word by word.
    /// It is appended when the body says nothing.
    #[test]
    fn what_was_typed_is_put_in() {
        assert_eq!(
            expand("review $ARGUMENTS now", "src/a.rs"),
            "review src/a.rs now"
        );
        assert_eq!(expand("old $ARGS form", "x"), "old x form");
        assert_eq!(
            expand("from $1 to $2, not $3", "main \"the branch\""),
            "from main to the branch, not "
        );
        assert_eq!(expand("check it", "carefully"), "check it\n\ncarefully");
        assert_eq!(
            expand("costs $5 dollars? no: $x", ""),
            "costs  dollars? no: $x"
        );
    }

    /// The settings are read, an agent is handed the prompt, and a wrong file says why.
    #[test]
    fn a_command_is_read_from_its_file() {
        let c = parse(
            "git:commit",
            "---\ndescription: commit it\nargument-hint: [scope]\nagent: scribe\n---\nCommit $1.",
            "here",
        )
        .unwrap();
        assert_eq!(c.argument_hint.as_deref(), Some("[scope]"));
        assert_eq!(c.prompt("ui"), "@scribe Commit ui.");
        assert!(parse("x", "---\ncolour: red\n---\nbody", "here").is_err());
        assert!(parse("x", "", "here").is_err());
    }

    /// A file in a directory is named after both.
    /// The project's command takes the place of the user's.
    #[test]
    fn commands_are_found_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user");
        let project = dir.path().join("project");
        std::fs::create_dir_all(user.join("git")).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(user.join("review.md"), "the user's").unwrap();
        std::fs::write(user.join("git").join("commit.md"), "commit").unwrap();
        std::fs::write(project.join("review.md"), "the project's").unwrap();
        std::fs::write(project.join("a.b.md"), "dotted").unwrap();
        let (commands, warnings) = load(&[user, project]);
        let names: Vec<_> = commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["git:commit", "review"]);
        assert_eq!(commands[1].body, "the project's");
        assert!(warnings.is_empty(), "{warnings:?}");
    }
}
