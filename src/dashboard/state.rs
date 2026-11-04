//! What the sessions of a server are doing, read off the messages a page would receive.
//!
//! A session is working from its first update until the end of its turn, waits for the person
//! while a question of lokode's is unanswered, and is idle otherwise.
//! Nothing here reads the clock or the network: the caller gives the time and the messages.

use std::collections::BTreeMap;
use std::time::Instant;

use serde_json::Value;

/// How much of the last thing a session said is kept.
const TEXT_KEPT: usize = 120;

/// What a session is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Status {
    /// Waiting for the person to answer a question.
    Waiting,
    /// A turn is running.
    Working,
    /// Nothing is running.
    Idle,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Waiting => "waiting",
            Status::Working => "working",
            Status::Idle => "idle",
        }
    }
}

/// A question of lokode's that nobody has answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    /// What it asks about.
    pub title: String,
    /// What answering "always" covers, in the words of the question.
    pub always: String,
}

/// One session of a server.
#[derive(Debug, Clone)]
pub struct SessionState {
    pub id: String,
    working: bool,
    /// What the session asks and nobody has answered, by the id of the question.
    pub asks: BTreeMap<i64, Ask>,
    pub turns: u32,
    /// The tool the session called last.
    pub tool: Option<String>,
    /// The end of what the session said last.
    pub said: String,
    /// The steps of the plan that are done, and all of them.
    pub plan: (usize, usize),
    /// Why the last turn stopped, when one has.
    pub stopped: Option<String>,
    /// When the session last did anything.
    pub active: Option<Instant>,
}

impl SessionState {
    pub fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            working: false,
            asks: BTreeMap::new(),
            turns: 0,
            tool: None,
            said: String::new(),
            plan: (0, 0),
            stopped: None,
            active: None,
        }
    }

    pub fn status(&self) -> Status {
        if !self.asks.is_empty() {
            Status::Waiting
        } else if self.working {
            Status::Working
        } else {
            Status::Idle
        }
    }

    /// The `update` of a `session/update` message.
    pub fn update(&mut self, update: &Value, now: Instant) {
        let kind = update
            .get("sessionUpdate")
            .and_then(|k| k.as_str())
            .unwrap_or_default();
        match kind {
            "_lokode/turn" => {
                self.working = false;
                self.turns += 1;
                self.stopped = update
                    .get("stopReason")
                    .and_then(|s| s.as_str())
                    .map(String::from);
                self.active = Some(now);
                return;
            }
            "user_message_chunk" => {
                self.working = true;
                self.stopped = None;
            }
            "agent_message_chunk" => {
                self.working = true;
                if let Some(text) = update.pointer("/content/text").and_then(|t| t.as_str()) {
                    self.said.push_str(text);
                    let excess = self.said.chars().count().saturating_sub(TEXT_KEPT);
                    self.said = self.said.chars().skip(excess).collect();
                }
            }
            "agent_thought_chunk" => self.working = true,
            "tool_call" => {
                self.working = true;
                self.tool = update
                    .get("title")
                    .and_then(|t| t.as_str())
                    .map(String::from);
            }
            "tool_call_update" => self.working = true,
            "plan" => {
                let entries = update
                    .get("entries")
                    .and_then(|e| e.as_array())
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let done = entries
                    .iter()
                    .filter(|e| e.get("status").and_then(|s| s.as_str()) == Some("completed"))
                    .count();
                self.plan = (done, entries.len());
            }
            _ => return,
        }
        self.active = Some(now);
    }
}

/// One server, and its sessions.
#[derive(Debug, Clone, Default)]
pub struct ServerState {
    pub root: String,
    pub version: String,
    pub sessions: BTreeMap<String, SessionState>,
}

impl ServerState {
    /// What `_lokode/hello` answered.
    pub fn hello(&mut self, result: &Value) {
        self.root = result["root"].as_str().unwrap_or_default().to_string();
        self.version = result["version"].as_str().unwrap_or_default().to_string();
        for id in result["open"].as_array().into_iter().flatten() {
            if let Some(id) = id.as_str() {
                self.sessions
                    .entry(id.to_string())
                    .or_insert_with(|| SessionState::new(id));
            }
        }
    }

    /// What `_lokode/attach` answered for `session`: everything it said, and what it waits on.
    pub fn attached(&mut self, session: &str, result: &Value, now: Instant) {
        let state = self
            .sessions
            .entry(session.to_string())
            .or_insert_with(|| SessionState::new(session));
        for update in result["updates"].as_array().into_iter().flatten() {
            state.update(update, now);
        }
        // What the replay left unfinished is not running: only the asks say what still waits.
        state.asks.clear();
        for ask in result["asks"].as_array().into_iter().flatten() {
            ask_of(ask, state);
        }
    }

    /// A message of the stream.
    pub fn message(&mut self, msg: &Value, now: Instant) {
        let method = msg.get("method").and_then(|m| m.as_str());
        match method {
            Some("session/update") => {
                let Some(session) = msg.pointer("/params/sessionId").and_then(|s| s.as_str())
                else {
                    return;
                };
                let state = self
                    .sessions
                    .entry(session.to_string())
                    .or_insert_with(|| SessionState::new(session));
                if let Some(update) = msg.pointer("/params/update") {
                    state.update(update, now);
                }
            }
            Some("session/request_permission") => {
                if let Some(session) = msg.pointer("/params/sessionId").and_then(|s| s.as_str()) {
                    let state = self
                        .sessions
                        .entry(session.to_string())
                        .or_insert_with(|| SessionState::new(session));
                    ask_of(msg, state);
                    state.active = Some(now);
                }
            }
            Some("_lokode/settled") => {
                if let Some(id) = msg.pointer("/params/id").and_then(|i| i.as_i64()) {
                    for state in self.sessions.values_mut() {
                        state.asks.remove(&id);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Note the question in `ask` on `state`.
fn ask_of(ask: &Value, state: &mut SessionState) {
    if let Some(id) = ask.get("id").and_then(|i| i.as_i64()) {
        let title = ask
            .pointer("/params/toolCall/title")
            .and_then(|t| t.as_str())
            .unwrap_or("a question")
            .to_string();
        let always = ask
            .pointer("/params/options")
            .and_then(|o| o.as_array())
            .into_iter()
            .flatten()
            .find(|o| o["kind"] == "allow_always")
            .and_then(|o| o["name"].as_str())
            .unwrap_or_default()
            .to_string();
        state.asks.insert(id, Ask { title, always });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn update(session: &str, update: Value) -> Value {
        json!({"method": "session/update", "params": {"sessionId": session, "update": update}})
    }

    #[test]
    fn a_session_works_from_its_first_update_to_the_end_of_its_turn() {
        let now = Instant::now();
        let mut server = ServerState::default();
        server.message(
            &update("s", json!({"sessionUpdate": "user_message_chunk"})),
            now,
        );
        assert_eq!(server.sessions["s"].status(), Status::Working);
        server.message(
            &update(
                "s",
                json!({"sessionUpdate": "tool_call", "title": "$ cargo test"}),
            ),
            now,
        );
        assert_eq!(server.sessions["s"].tool.as_deref(), Some("$ cargo test"));
        server.message(
            &update(
                "s",
                json!({"sessionUpdate": "_lokode/turn", "stopReason": "end_turn"}),
            ),
            now,
        );
        let s = &server.sessions["s"];
        assert_eq!(s.status(), Status::Idle);
        assert_eq!((s.turns, s.stopped.as_deref()), (1, Some("end_turn")));
    }

    #[test]
    fn a_question_makes_a_session_wait_until_it_is_settled() {
        let now = Instant::now();
        let mut server = ServerState::default();
        server.message(&update("s", json!({"sessionUpdate": "tool_call"})), now);
        server.message(
            &json!({"id": 4, "method": "session/request_permission", "params": {
                "sessionId": "s", "toolCall": {"title": "$ rm -r build"},
                "options": [{"optionId": "always", "kind": "allow_always", "name": "Always: this command"}]}}),
            now,
        );
        assert_eq!(server.sessions["s"].status(), Status::Waiting);
        assert_eq!(server.sessions["s"].asks[&4].title, "$ rm -r build");
        assert_eq!(server.sessions["s"].asks[&4].always, "Always: this command");
        server.message(
            &json!({"method": "_lokode/settled", "params": {"id": 4}}),
            now,
        );
        assert_eq!(server.sessions["s"].status(), Status::Working);
    }

    #[test]
    fn what_a_session_said_keeps_its_end_and_the_plan_counts_the_steps_done() {
        let now = Instant::now();
        let mut s = SessionState::new("s");
        s.update(
            &json!({"sessionUpdate": "agent_message_chunk", "content": {"text": "a".repeat(500)}}),
            now,
        );
        s.update(
            &json!({"sessionUpdate": "agent_message_chunk", "content": {"text": "END"}}),
            now,
        );
        assert_eq!(s.said.chars().count(), TEXT_KEPT);
        assert!(s.said.ends_with("END"));
        s.update(
            &json!({"sessionUpdate": "plan", "entries": [
                {"status": "completed"}, {"status": "in_progress"}, {"status": "pending"}]}),
            now,
        );
        assert_eq!(s.plan, (1, 3));
    }

    #[test]
    fn a_session_opened_later_is_rebuilt_from_what_it_said_and_what_it_waits_on() {
        let now = Instant::now();
        let mut server = ServerState::default();
        server.hello(&json!({"root": "/p", "version": "1", "open": ["s", "t"]}));
        assert_eq!(server.sessions.len(), 2);
        server.attached(
            "s",
            &json!({
                "updates": [
                    {"sessionUpdate": "user_message_chunk"},
                    {"sessionUpdate": "tool_call", "title": "$ ls"},
                ],
                "asks": [{"id": 9, "method": "session/request_permission",
                          "params": {"sessionId": "s", "toolCall": {"title": "$ ls"}}}],
            }),
            now,
        );
        assert_eq!(server.sessions["s"].status(), Status::Waiting);
        assert_eq!(server.sessions["t"].status(), Status::Idle);
        assert_eq!(server.root, "/p");
    }

    #[test]
    fn waiting_comes_before_working_before_idle() {
        assert!(Status::Waiting < Status::Working && Status::Working < Status::Idle);
    }
}
