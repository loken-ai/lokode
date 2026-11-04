//! What a person can ask of a session from the dashboard, as the messages a page would post.

use serde_json::{json, Value};

/// The answer to the question `ask`, with one of its options: `once`, `always` or `no`.
pub fn answer(ask: i64, option: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": ask,
        "result": {"outcome": {"outcome": "selected", "optionId": option}},
    })
}

/// A message to `session`, under an id of the dashboard's own.
pub fn prompt(session: &str, text: &str, id: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "session/prompt",
        "params": {"sessionId": session, "prompt": [{"type": "text", "text": text}]},
    })
}

/// The end of the turn `session` is in.
pub fn cancel(session: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "session/cancel",
        "params": {"sessionId": session},
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_names_the_question_and_the_option() {
        let a = answer(4, "once");
        assert_eq!(a["id"], 4);
        assert_eq!(a["result"]["outcome"]["optionId"], "once");
        assert!(a.get("method").is_none());
    }

    #[test]
    fn a_message_and_a_stop_are_what_a_page_posts() {
        let p = prompt("s", "go on", "dash-1");
        assert_eq!(p["method"], "session/prompt");
        assert_eq!(p["params"]["prompt"][0]["text"], "go on");
        let c = cancel("s");
        assert_eq!(c["method"], "session/cancel");
        assert!(c.get("id").is_none());
    }
}
