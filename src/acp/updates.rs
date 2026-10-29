//! What the engine says during a turn, as the session updates an editor reads.
//!
//! | Engine | Update |
//! |---|---|
//! | streamed text, the finished answer | `agent_message_chunk`, never the same text twice |
//! | reasoning, a notice | `agent_thought_chunk` |
//! | a tool call and its result | `tool_call`, then `tool_call_update`, an edit with its diff |
//! | a line typed during the turn | `user_message_chunk` |
//! | a change of mode | `current_mode_update` |
//! | the task list | `plan`, sent whole whenever it changes |
//!
//! What ACP has no place for, the turn's receipt, the window and the energy, goes in the
//! `_meta` of the answer to the prompt, under `lokode`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::agent::agent_loop::{AgentEvent, Mode};
use crate::tools::plan::{State, Task};

/// Characters of a tool's output sent to the editor.
const SHOWN_OUTPUT: usize = 20_000;

/// The kind an editor gives a tool call, by the tool's name.
pub fn kind(tool: &str) -> &'static str {
    match tool {
        "read_file" | "list_dir" | "skill" => "read",
        "write_file" | "edit_file" | "edit_lines" | "apply_patch" => "edit",
        "bash" | "run_tests" => "execute",
        "grep"
        | "glob"
        | "lsp_definition"
        | "lsp_references"
        | "lsp_implementations"
        | "lsp_hover"
        | "lsp_calls"
        | "lsp_symbols"
        | "lsp_diagnostics" => "search",
        "web_fetch" => "fetch",
        "update_plan" | "delegate" => "think",
        "submit_plan" => "switch_mode",
        _ => "other",
    }
}

/// The id of a mode as an editor names it, and the mode back.
pub fn mode_id(mode: Mode, accepting_writes: bool) -> &'static str {
    match (mode, accepting_writes) {
        (Mode::Plan, _) => "plan",
        (Mode::Act, false) => "act",
        (Mode::Act, true) => "accept-edits",
    }
}

/// The modes offered, with the one in force.
pub fn modes(current: &str) -> Value {
    json!({
        "currentModeId": current,
        "availableModes": [
            {"id": "act", "name": "Act", "description": "Change things, asking before each change"},
            {"id": "accept-edits", "name": "Accept edits", "description": "Write files without asking, still asking before commands"},
            {"id": "plan", "name": "Plan", "description": "Read and search, change nothing, end with a plan to approve"},
        ]
    })
}

/// A text chunk of `kind`.
fn chunk(kind: &str, text: &str) -> Value {
    json!({"sessionUpdate": kind, "content": {"type": "text", "text": text}})
}

/// The task list as a plan.
pub fn plan(tasks: &[Task]) -> Value {
    let entries: Vec<Value> = tasks
        .iter()
        .map(|t| {
            let status = match t.state {
                State::Todo => "pending",
                State::Doing => "in_progress",
                State::Done => "completed",
            };
            json!({"content": t.what, "priority": "medium", "status": status})
        })
        .collect();
    json!({"sessionUpdate": "plan", "entries": entries})
}

/// What one turn has said so far, to say the rest of it once.
pub struct Translator {
    root: PathBuf,
    /// The text of the answer being streamed.
    streamed: String,
    /// The files an edit is about to change, with what they held, by call.
    before: HashMap<String, Vec<(PathBuf, Option<String>)>>,
    receipt: Option<crate::agent::agent_loop::Receipt>,
    window: Option<crate::agent::session::Occupancy>,
    engine: Option<crate::agent::agent_loop::Engine>,
    rate: Option<f64>,
}

impl Translator {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            streamed: String::new(),
            before: HashMap::new(),
            receipt: None,
            window: None,
            engine: None,
            rate: None,
        }
    }

    /// What the turn came to, for the `_meta` of the answer, and the turn's receipt
    /// forgotten so the next turn starts without it.
    pub fn meta(&mut self) -> Value {
        let mut lokode = serde_json::Map::new();
        if let Some(r) = self.receipt.take() {
            lokode.insert(
                "turn".into(),
                json!({
                    "tookMs": r.took.as_millis() as u64,
                    "steps": r.steps,
                    "tokensIn": r.tokens_in,
                    "tokensOut": r.tokens_out,
                    "files": r.files,
                    "joules": r.energy.map(|e| e.joules),
                }),
            );
        }
        if let Some(w) = &self.window {
            lokode.insert(
                "window".into(),
                json!({"percent": w.percent, "used": w.used, "budget": w.budget}),
            );
        }
        if let Some(e) = &self.engine {
            lokode.insert(
                "energy".into(),
                json!({"turnJoules": e.turn.joules, "sessionJoules": e.session.joules}),
            );
        }
        if let Some(rate) = self.rate {
            lokode.insert("tokensPerSecond".into(), json!(rate));
        }
        json!({"lokode": lokode})
    }

    /// The updates `event` makes, none for what an editor has no place for.
    pub fn apply(&mut self, event: &AgentEvent) -> Vec<Value> {
        match event {
            AgentEvent::AssistantDelta(text) => {
                self.streamed.push_str(text);
                vec![chunk("agent_message_chunk", text)]
            }
            AgentEvent::AssistantText(text) => {
                let streamed = std::mem::take(&mut self.streamed);
                // The finished answer settles what streamed: only what did not stream yet
                // is sent, since a chunk cannot be taken back.
                let rest = match text.strip_prefix(streamed.as_str()) {
                    Some(rest) => rest,
                    None if streamed.is_empty() => text.as_str(),
                    None => "",
                };
                match rest.trim().is_empty() {
                    true => Vec::new(),
                    false => vec![chunk("agent_message_chunk", rest)],
                }
            }
            AgentEvent::Reasoning(text) => vec![chunk("agent_thought_chunk", text)],
            AgentEvent::Receipt(r) => {
                self.receipt = Some(r.clone());
                Vec::new()
            }
            AgentEvent::Window(w) => {
                self.window = Some(w.clone());
                Vec::new()
            }
            AgentEvent::Engine(e) => {
                self.engine = Some(*e);
                Vec::new()
            }
            AgentEvent::Rate(r) => {
                self.rate = Some(*r);
                Vec::new()
            }
            AgentEvent::Notice(text) => vec![chunk("agent_thought_chunk", &format!("{text}\n"))],
            AgentEvent::Steered(text) => vec![chunk("user_message_chunk", text)],
            AgentEvent::Mode(mode) => vec![json!({
                "sessionUpdate": "current_mode_update",
                "currentModeId": mode_id(*mode, false),
            })],
            AgentEvent::ToolCall {
                id,
                name,
                args_pretty,
            } => {
                self.streamed.clear();
                let args: Value = serde_json::from_str(args_pretty).unwrap_or(Value::Null);
                let paths = self.paths(name, &args);
                if kind(name) == "edit" {
                    let held = paths
                        .iter()
                        .map(|p| (p.clone(), std::fs::read_to_string(p).ok()))
                        .collect();
                    self.before.insert(id.clone(), held);
                }
                let locations: Vec<Value> = paths
                    .iter()
                    .map(|p| json!({"path": p, "line": args.get("start_line").or_else(|| args.get("line")).and_then(|l| l.as_u64())}))
                    .collect();
                vec![json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": id,
                    "title": title(name, &args, &self.root),
                    "kind": kind(name),
                    "status": "in_progress",
                    "locations": locations,
                    "rawInput": args,
                })]
            }
            AgentEvent::ToolResult { id, output, .. } => {
                let mut content = Vec::new();
                if !output.is_error {
                    for (path, old) in self.before.remove(id).unwrap_or_default() {
                        let new = std::fs::read_to_string(&path).ok();
                        if let Some(new) = new.filter(|n| Some(n) != old.as_ref()) {
                            content.push(json!({
                                "type": "diff", "path": path, "oldText": old, "newText": new,
                            }));
                        }
                    }
                } else {
                    self.before.remove(id);
                }
                let text: String = output.content.chars().take(SHOWN_OUTPUT).collect();
                if !text.trim().is_empty() {
                    content.push(
                        json!({"type": "content", "content": {"type": "text", "text": text}}),
                    );
                }
                vec![json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": id,
                    "status": if output.is_error { "failed" } else { "completed" },
                    "content": content,
                })]
            }
            _ => Vec::new(),
        }
    }

    /// The files a call names, made absolute against the project.
    fn paths(&self, tool: &str, args: &Value) -> Vec<PathBuf> {
        let named = match tool {
            "apply_patch" => Vec::new(),
            _ => ["path", "file"]
                .iter()
                .filter_map(|k| args.get(*k).and_then(|v| v.as_str()))
                .take(1)
                .collect(),
        };
        named
            .into_iter()
            .map(|p| match Path::new(p).is_absolute() {
                true => PathBuf::from(p),
                false => self.root.join(p),
            })
            .collect()
    }
}

/// A line naming the call: the tool and what it works on.
fn title(tool: &str, args: &Value, root: &Path) -> String {
    let text = |k: &str| args.get(k).and_then(|v| v.as_str());
    let about = match tool {
        "bash" => text("cmd").map(str::to_string),
        "grep" | "glob" => text("pattern").map(str::to_string),
        "web_fetch" => text("url").map(str::to_string),
        "delegate" => text("task").map(str::to_string),
        _ => text("path").or_else(|| text("file")).map(|p| {
            Path::new(p)
                .strip_prefix(root)
                .map(|r| r.display().to_string())
                .unwrap_or_else(|_| p.to_string())
        }),
    };
    match about {
        Some(about) => {
            let short: String = about.chars().take(120).collect();
            format!("{tool} {short}")
        }
        None => tool.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::agent_loop::Origin;
    use crate::tools::registry::ToolOutput;

    /// Streamed text is sent as it comes, and the finished answer adds only what did not
    /// stream.
    #[test]
    fn an_answer_is_sent_once() {
        let mut t = Translator::new(Path::new("/p"));
        let a = t.apply(&AgentEvent::AssistantDelta("Hel".into()));
        assert_eq!(a[0]["content"]["text"], "Hel");
        let b = t.apply(&AgentEvent::AssistantText("Hello.".into()));
        assert_eq!(b[0]["content"]["text"], "lo.");
        let c = t.apply(&AgentEvent::AssistantText("Whole.".into()));
        assert_eq!(c[0]["content"]["text"], "Whole.");
        t.apply(&AgentEvent::AssistantDelta("one way".into()));
        assert!(t
            .apply(&AgentEvent::AssistantText("another".into()))
            .is_empty());
    }

    /// An edit is announced with its file and comes back with its diff.
    #[test]
    fn an_edit_carries_its_diff() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "old\n").unwrap();
        let mut t = Translator::new(dir.path());
        let call = t.apply(&AgentEvent::ToolCall {
            id: "c1".into(),
            name: "edit_file".into(),
            args_pretty: r#"{"path": "a.rs", "old": "old", "new": "new"}"#.into(),
        });
        assert_eq!(call[0]["kind"], "edit");
        assert_eq!(call[0]["title"], "edit_file a.rs");
        assert_eq!(
            call[0]["locations"][0]["path"],
            dir.path().join("a.rs").display().to_string()
        );
        std::fs::write(dir.path().join("a.rs"), "new\n").unwrap();
        let done = t.apply(&AgentEvent::ToolResult {
            id: "c1".into(),
            name: "edit_file".into(),
            output: ToolOutput::ok("edited"),
            took: std::time::Duration::ZERO,
            origin: Origin::Ran,
        });
        assert_eq!(done[0]["status"], "completed");
        assert_eq!(done[0]["content"][0]["type"], "diff");
        assert_eq!(done[0]["content"][0]["oldText"], "old\n");
        assert_eq!(done[0]["content"][0]["newText"], "new\n");
        assert_eq!(done[0]["content"][1]["content"]["text"], "edited");
    }

    #[test]
    fn the_task_list_is_a_plan() {
        let tasks = vec![
            Task {
                what: "read".into(),
                state: State::Done,
            },
            Task {
                what: "fix".into(),
                state: State::Doing,
            },
        ];
        let p = plan(&tasks);
        assert_eq!(p["entries"][0]["status"], "completed");
        assert_eq!(p["entries"][1]["status"], "in_progress");
        assert_eq!(modes("plan")["availableModes"][2]["id"], "plan");
    }
}
