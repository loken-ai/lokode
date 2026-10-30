//! The providers lokode knows by name.
//!
//! Each preset holds where the provider is, what it speaks and where its key is usually kept.
//!
//! A provider is named with `provider = "openrouter"` in `[backend]` or in a `[backends]` entry.
//! It can also be named with `--provider`.
//! A `url`, a `kind` or an `api-key-env` written beside it wins over the preset's.
//! Any other OpenAI-compatible server is reached by its `url` alone.

use crate::config::BackendKind;

/// One provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    pub name: &'static str,
    pub url: &'static str,
    pub kind: BackendKind,
    /// The variable its key is usually kept in.
    /// `None` for a server on this machine.
    pub key_env: Option<&'static str>,
}

const fn hosted(name: &'static str, url: &'static str, key_env: &'static str) -> Preset {
    Preset {
        name,
        url,
        kind: BackendKind::Openai,
        key_env: Some(key_env),
    }
}

const fn local(name: &'static str, url: &'static str) -> Preset {
    Preset {
        name,
        url,
        kind: BackendKind::Openai,
        key_env: None,
    }
}

pub const PRESETS: &[Preset] = &[
    Preset {
        name: "anthropic",
        url: "https://api.anthropic.com",
        kind: BackendKind::Anthropic,
        key_env: Some("ANTHROPIC_API_KEY"),
    },
    hosted("openai", "https://api.openai.com/v1", "OPENAI_API_KEY"),
    hosted(
        "openrouter",
        "https://openrouter.ai/api/v1",
        "OPENROUTER_API_KEY",
    ),
    hosted(
        "gemini",
        "https://generativelanguage.googleapis.com/v1beta/openai",
        "GEMINI_API_KEY",
    ),
    hosted("mistral", "https://api.mistral.ai/v1", "MISTRAL_API_KEY"),
    hosted("groq", "https://api.groq.com/openai/v1", "GROQ_API_KEY"),
    hosted(
        "deepseek",
        "https://api.deepseek.com/v1",
        "DEEPSEEK_API_KEY",
    ),
    hosted("xai", "https://api.x.ai/v1", "XAI_API_KEY"),
    hosted(
        "together",
        "https://api.together.xyz/v1",
        "TOGETHER_API_KEY",
    ),
    hosted(
        "fireworks",
        "https://api.fireworks.ai/inference/v1",
        "FIREWORKS_API_KEY",
    ),
    hosted("cerebras", "https://api.cerebras.ai/v1", "CEREBRAS_API_KEY"),
    hosted(
        "deepinfra",
        "https://api.deepinfra.com/v1/openai",
        "DEEPINFRA_API_KEY",
    ),
    local("loken", "http://127.0.0.1:11435"),
    local("ollama", "http://127.0.0.1:11434/v1"),
    local("lmstudio", "http://127.0.0.1:1234/v1"),
    local("llamacpp", "http://127.0.0.1:8080/v1"),
    local("vllm", "http://127.0.0.1:8000/v1"),
];

/// The provider named `name`.
pub fn find(name: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.name.eq_ignore_ascii_case(name))
}

/// The provider named `name`, or why there is none.
pub fn named(name: &str) -> Result<&'static Preset, String> {
    find(name).ok_or_else(|| {
        format!(
            "lokode knows no provider {name:?}: it knows {}. Name the server's url instead",
            PRESETS
                .iter()
                .map(|p| p.name)
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

/// The address of `path` on an OpenAI-compatible server at `base`.
/// A base that already ends on its API's root, `/v1` or `/openai`, takes the path as it is.
/// A bare host takes it under `/v1`.
pub fn endpoint(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    let rooted = base.ends_with("/v1") || base.ends_with("/openai");
    match rooted {
        true => format!("{base}/{path}"),
        false => format!("{base}/v1/{path}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_lands_under_the_apis_root() {
        assert_eq!(
            endpoint("http://127.0.0.1:11435", "chat/completions"),
            "http://127.0.0.1:11435/v1/chat/completions"
        );
        assert_eq!(
            endpoint("https://api.openai.com/v1/", "models"),
            "https://api.openai.com/v1/models"
        );
        assert_eq!(
            endpoint(find("gemini").unwrap().url, "chat/completions"),
            "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions"
        );
        assert_eq!(
            endpoint(find("deepinfra").unwrap().url, "models"),
            "https://api.deepinfra.com/v1/openai/models"
        );
    }

    #[test]
    fn a_provider_is_found_by_name() {
        assert_eq!(
            find("OpenRouter").unwrap().key_env,
            Some("OPENROUTER_API_KEY")
        );
        assert_eq!(find("anthropic").unwrap().kind, BackendKind::Anthropic);
        assert!(named("nowhere").unwrap_err().contains("openrouter"));
        let mut names: Vec<&str> = PRESETS.iter().map(|p| p.name).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), PRESETS.len(), "a name is used twice");
    }
}
