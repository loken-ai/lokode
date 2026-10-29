//! The block of settings at the head of a Markdown file: `key: value` lines between two
//! `---` lines. Agents, commands and skills are written this way.

/// The settings of `text`, in order, and the rest of it. A file that does not open on
/// `---` has no settings.
pub fn split(text: &str) -> Result<(Vec<(String, String)>, String), String> {
    let text = text.replace("\r\n", "\n");
    let (head, body) = match text.strip_prefix("---\n") {
        Some(rest) => match rest.split_once("\n---\n") {
            Some((head, body)) => (head.to_string(), body.to_string()),
            None => match rest.strip_suffix("\n---") {
                Some(head) => (head.to_string(), String::new()),
                None => return Err("its settings have no closing ---".into()),
            },
        },
        None => (String::new(), text.clone()),
    };
    let mut settings = Vec::new();
    for line in head
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        let (key, value) = line
            .split_once(':')
            .ok_or_else(|| format!("the line {line:?} is not key: value"))?;
        let value = value.trim().trim_matches('"').to_string();
        settings.push((key.trim().to_string(), value));
    }
    Ok((settings, body.trim().to_string()))
}

/// Whether `name` is made of letters, digits, `-` and `_` only, as a file named after a
/// command, an agent or a skill must be.
pub fn is_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_and_body_are_told_apart() {
        let (settings, body) = split("---\na: 1\n# note\nb: \"two\"\n---\nthe body\n").unwrap();
        assert_eq!(
            settings,
            [
                ("a".to_string(), "1".to_string()),
                ("b".into(), "two".into())
            ]
        );
        assert_eq!(body, "the body");
        let (settings, body) = split("no head").unwrap();
        assert!(settings.is_empty());
        assert_eq!(body, "no head");
        assert!(split("---\na: 1\n").is_err());
        assert!(split("---\nloose\n---\nx").is_err());
        assert!(is_name("git-commit_2") && !is_name("a.b") && !is_name(""));
    }
}
