//! What window the backend actually offers for a model.
//!
//! A client that guesses is a client that is wrong in both directions. Guessing small
//! wastes the window that is there; guessing large sends prompts the server has to cut,
//! and a prompt cut in the middle produces an answer that is fluent and wrong about what
//! it was asked. The default here was twice what the server offered, and every request
//! went out too big without either side saying so.
//!
//! So it is asked. The OpenAI surface has nowhere to say it, but the Ollama one does, and
//! a backend that says nothing is simply left alone.

use std::time::Duration;

/// How long to wait. Asked once, at startup, of a server on this machine or this network.
const TIMEOUT: Duration = Duration::from_secs(3);

/// The window `model` is loaded with, when the backend says.
pub async fn of(http: &reqwest::Client, base: &str, model: &str) -> Option<usize> {
    let url = format!("{}/api/models/loaded", base.trim_end_matches('/'));
    let body = http
        .get(&url)
        .timeout(TIMEOUT)
        .send()
        .await
        .ok()?
        .json::<serde_json::Value>()
        .await
        .ok()?;
    from_listing(&body, model)
}

/// The context length for `model` in a loaded-models listing.
fn from_listing(body: &serde_json::Value, model: &str) -> Option<usize> {
    body.get("models")?.as_array()?.iter().find_map(|m| {
        // A model listed in parts carries its name and then the part in brackets.
        let name = m.get("model")?.as_str()?;
        let name = name.split(" (").next().unwrap_or(name).trim();
        (name == model)
            .then(|| m.get("context_length")?.as_u64())
            .flatten()
            .map(|n| n as usize)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_is_read_for_the_model_that_is_asked_about() {
        let body = serde_json::json!({"models": [
            {"model": "small", "context_length": 4096},
            {"model": "big", "context_length": 32768},
        ]});
        assert_eq!(from_listing(&body, "big"), Some(32768));
        assert_eq!(from_listing(&body, "small"), Some(4096));
        assert_eq!(from_listing(&body, "absent"), None);
    }

    /// A model listed in parts is one model, and its window is stated once.
    #[test]
    fn a_model_listed_in_parts_still_answers() {
        let body = serde_json::json!({"models": [
            {"model": "wan (transformer)", "context_length": 8192},
        ]});
        assert_eq!(from_listing(&body, "wan"), Some(8192));
    }

    /// A backend that says nothing is a backend nothing is assumed about.
    #[test]
    fn a_listing_without_a_window_says_nothing() {
        let body = serde_json::json!({"models": [{"model": "big"}]});
        assert_eq!(from_listing(&body, "big"), None);
        assert_eq!(from_listing(&serde_json::json!({}), "big"), None);
    }
}
