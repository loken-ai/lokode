//! Skills: know-how the model loads when it needs it.
//!
//! A skill is a directory holding a `SKILL.md`, in `~/.config/lokode/skills/` or in the
//! project's `.lokode/skills/`, named by its directory, the project's taking the place of
//! the user's. Its file opens with a description between 2 `---` lines, and the rest is
//! the know-how. The model sees each skill's name and description in the tool's
//! description and loads one by name, or a file the skill keeps beside it. A window holds
//! what is used, not everything that could be.

use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::tools::registry::{Tool, ToolCtx, ToolOutput};

/// One skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// The directory it lives in.
    pub dir: PathBuf,
}

/// The directories skills are read from: the user's, the plugins', then the project's.
pub fn dirs(root: &Path) -> Vec<PathBuf> {
    let user = crate::config::user_config_path().and_then(|p| p.parent().map(|d| d.join("skills")));
    user.into_iter()
        .chain(crate::plugins::dirs("skills"))
        .chain([root.join(".lokode").join("skills")])
        .collect()
}

/// The skills of `dirs`, a later directory's taking the place of an earlier one's. Says
/// what it could not read.
pub fn load(dirs: &[PathBuf]) -> (Vec<Skill>, Vec<String>) {
    let mut out: Vec<Skill> = Vec::new();
    let mut warnings = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut found: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.join("SKILL.md").is_file())
            .collect();
        found.sort();
        for skill_dir in found {
            let name = skill_dir
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if !crate::frontmatter::is_name(&name) {
                warnings.push(format!(
                    "the skill {} is not named with letters, digits, - and _ only",
                    skill_dir.display()
                ));
                continue;
            }
            match read(&skill_dir) {
                Ok((description, _)) => {
                    out.retain(|s| s.name != name);
                    out.push(Skill {
                        name,
                        description,
                        dir: skill_dir,
                    });
                }
                Err(why) => warnings.push(format!("the skill {}: {why}", skill_dir.display())),
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    (out, warnings)
}

/// A skill's description and know-how.
fn read(dir: &Path) -> Result<(String, String), String> {
    let text = std::fs::read_to_string(dir.join("SKILL.md")).map_err(|e| e.to_string())?;
    // A skill of a plugin is at `<plugin>/skills/<name>`.
    let text = crate::claude::in_plugin(&text, dir.parent().unwrap_or(dir));
    let (settings, body) = crate::frontmatter::split(&text)?;
    let mut description = String::new();
    for (key, value) in settings {
        match key.as_str() {
            "description" => description = value,
            // Written by other tools' skills, and harmless here.
            "name"
            | "license"
            | "metadata"
            | "allowed-tools"
            | "model"
            | "version"
            | "disable-model-invocation"
            | "user-invocable"
            | "argument-hint" => {}
            other => return Err(format!("it has a setting {other:?} lokode does not know")),
        }
    }
    if description.is_empty() {
        return Err("it has no description, which is what the model chooses it by".into());
    }
    if body.is_empty() {
        return Err("it says nothing past its description".into());
    }
    Ok((description, body))
}

/// The files a skill keeps beside its `SKILL.md`, relative to its directory.
fn kept(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = ignore::WalkBuilder::new(dir)
        .max_depth(Some(3))
        .build()
        .flatten()
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .filter_map(|e| {
            let rel = e
                .path()
                .strip_prefix(dir)
                .ok()?
                .to_string_lossy()
                .to_string();
            (rel != "SKILL.md").then_some(rel)
        })
        .collect();
    out.sort();
    out
}

/// The tool that loads a skill.
pub struct LoadSkill {
    skills: Vec<Skill>,
    description: String,
}

impl LoadSkill {
    /// The tool for `skills`, or none when there is no skill.
    pub fn new(skills: Vec<Skill>) -> Option<Self> {
        if skills.is_empty() {
            return None;
        }
        let listed: Vec<String> = skills
            .iter()
            .map(|s| format!("{} ({})", s.name, s.description))
            .collect();
        let description = format!(
            "Load a skill: know-how for a kind of task, to read before doing it. Name the skill, \
             or one of the files it keeps with file. Skills: {}.",
            listed.join("; ")
        );
        Some(Self {
            skills,
            description,
        })
    }
}

#[derive(Deserialize)]
struct Args {
    name: String,
    #[serde(default)]
    file: Option<String>,
}

#[async_trait]
impl Tool for LoadSkill {
    fn name(&self) -> &str {
        "skill"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn args_schema(&self) -> Value {
        let names: Vec<&str> = self.skills.iter().map(|s| s.name.as_str()).collect();
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "enum": names},
                "file": {"type": "string"}
            },
            "required": ["name"],
            "additionalProperties": false
        })
    }

    async fn run(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput> {
        let a: Args = serde_json::from_value(args)?;
        let Some(skill) = self.skills.iter().find(|s| s.name == a.name) else {
            return Ok(ToolOutput::err(format!("there is no skill {}", a.name)));
        };
        match a.file {
            None => {
                let (_, body) = match read(&skill.dir) {
                    Ok(r) => r,
                    Err(why) => return Ok(ToolOutput::err(format!("{}: {why}", skill.name))),
                };
                let files = kept(&skill.dir);
                let text = match files.is_empty() {
                    true => body,
                    false => format!(
                        "{body}\n\nFiles it keeps, loaded with file: {}",
                        files.join(", ")
                    ),
                };
                Ok(ToolOutput::ok(ctx.clamp(text)))
            }
            Some(file) => {
                // Only what the skill keeps: a name that leaves its directory is refused.
                let path = skill.dir.join(&file);
                let inside = path
                    .canonicalize()
                    .ok()
                    .zip(skill.dir.canonicalize().ok())
                    .is_some_and(|(p, d)| p.starts_with(d) && p.is_file());
                if !inside {
                    return Ok(ToolOutput::err(format!(
                        "{} keeps no file {file}; it keeps {}",
                        skill.name,
                        kept(&skill.dir).join(", ")
                    )));
                }
                match std::fs::read_to_string(&path) {
                    Ok(text) => Ok(ToolOutput::ok(ctx.clamp(text))),
                    Err(e) => Ok(ToolOutput::err(format!("{file}: {e}"))),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::harness::{Answer, Harness};

    fn write_skill(dir: &Path, name: &str, text: &str) {
        let d = dir.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("SKILL.md"), text).unwrap();
    }

    /// The project's skill takes the place of the user's, and one without a description
    /// is refused with a word.
    #[test]
    fn skills_are_found_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user");
        let project = dir.path().join("project");
        write_skill(&user, "release", "---\ndescription: the user's\n---\nsteps");
        write_skill(
            &user,
            "pdf",
            "---\ndescription: read a pdf\n---\nuse pdftotext",
        );
        write_skill(
            &project,
            "release",
            "---\ndescription: the project's\n---\nsteps",
        );
        write_skill(&project, "bare", "no description");
        let (skills, warnings) = load(&[user, project]);
        let names: Vec<_> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["pdf", "release"]);
        assert_eq!(skills[1].description, "the project's");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
    }

    /// Loading a skill gives its know-how and what it keeps, and a kept file loads; a file
    /// outside it does not.
    #[tokio::test]
    async fn a_skill_loads_with_its_files() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(
            dir.path(),
            "pdf",
            "---\ndescription: read a pdf\n---\nuse pdftotext",
        );
        std::fs::create_dir_all(dir.path().join("pdf/scripts")).unwrap();
        std::fs::write(dir.path().join("pdf/scripts/extract.sh"), "pdftotext $1").unwrap();
        std::fs::write(dir.path().join("secret"), "no").unwrap();
        let (skills, _) = load(&[dir.path().to_path_buf()]);
        let tool = LoadSkill::new(skills).unwrap();
        assert!(tool.description().contains("pdf (read a pdf)"));
        let h = Harness::new(Answer::Yes);
        let out = tool.run(&h.ctx, json!({"name": "pdf"})).await.unwrap();
        assert!(out.content.contains("use pdftotext"), "{}", out.content);
        assert!(
            out.content.contains("scripts/extract.sh"),
            "{}",
            out.content
        );
        let out = tool
            .run(&h.ctx, json!({"name": "pdf", "file": "scripts/extract.sh"}))
            .await
            .unwrap();
        assert_eq!(out.content, "pdftotext $1");
        let out = tool
            .run(&h.ctx, json!({"name": "pdf", "file": "../secret"}))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(LoadSkill::new(Vec::new()).is_none());
    }
}
