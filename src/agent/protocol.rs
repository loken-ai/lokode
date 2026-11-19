//! How a turn's tool calls travel, and the one shape the loop reads them in.
//!
//! 2 ways of asking the same thing.
//!
//! The envelope makes the model emit a single JSON object, constrained by a grammar built from the
//! tool schemas.
//! The native protocol uses the server's own `tools` field and gets back `tool_calls`.
//!
//! The envelope serves a server that will not do native tool calls.
//! It also serves a model that does them badly.
//! Both still work against a grammar.
//!
//! The loop sees neither.
//! It gets a [`ModelTurn`].
//! Dispatch, approval and the transcript are the same either way.

use serde::Deserialize;
use serde_json::Value;

/// Which way this session talks to the model about tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolProtocol {
    /// The server's `tools` field and `tool_calls` in the reply.
    #[default]
    Native,
    /// A single JSON object per turn, constrained by a generated grammar.
    Envelope,
}

impl ToolProtocol {
    pub fn label(self) -> &'static str {
        match self {
            ToolProtocol::Native => "native",
            ToolProtocol::Envelope => "envelope",
        }
    }
}

/// The protocol in force, shared by everything that opens a session of its own.
///
/// Which protocol works is a property of the backend, not of one session.
/// It is not known until something has asked.
///
/// A delegated turn or a background question started after the answer came in speaks what the
/// main session settled on.
/// Otherwise it works with tools the server never passes on.
#[derive(Clone)]
pub struct SharedProtocol(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl SharedProtocol {
    pub fn new(protocol: ToolProtocol) -> Self {
        let shared = Self(Default::default());
        shared.set(protocol);
        shared
    }

    pub fn get(&self) -> ToolProtocol {
        if self.0.load(std::sync::atomic::Ordering::Relaxed) {
            ToolProtocol::Envelope
        } else {
            ToolProtocol::Native
        }
    }

    pub fn set(&self, protocol: ToolProtocol) {
        self.0.store(
            protocol == ToolProtocol::Envelope,
            std::sync::atomic::Ordering::Relaxed,
        );
    }
}

impl Default for SharedProtocol {
    fn default() -> Self {
        Self::new(ToolProtocol::Native)
    }
}

/// What the model asked for this turn, however it was asked.
#[derive(Debug, Clone, Default)]
pub struct ModelTurn {
    pub text: String,
    pub calls: Vec<ResolvedCall>,
    /// The answer was empty and its reasoning holds what looks like a call that could not be read.
    pub stranded: bool,
}

/// One call, with its arguments parsed and its identity settled.
#[derive(Debug, Clone)]
pub struct ResolvedCall {
    /// The id the result must be returned against.
    ///
    /// In the native protocol this is the server's own id.
    /// A `role: "tool"` message answering an id nobody issued is rejected by a strict server.
    /// Each result is therefore returned against the id of the call it answers.
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// The envelope protocol's per-turn object.
/// Mirrors `build_turn_schema`.
#[derive(Debug, Clone, Deserialize)]
pub struct TurnEnvelope {
    #[serde(default)]
    pub assistant_text: String,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallEnvelope>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolCallEnvelope {
    pub name: String,
    pub arguments: Value,
}

/// Tool-call fragments arriving out of a stream, reassembled by index.
#[derive(Debug, Default)]
pub struct CallAccumulator {
    parts: Vec<Part>,
}

#[derive(Debug, Default, Clone)]
struct Part {
    id: Option<String>,
    name: String,
    arguments: String,
}

impl CallAccumulator {
    /// Take in one fragment.
    ///
    /// The index is the only thing tying fragments together.
    /// The name arrives once and the arguments arrive a few characters at a time.
    /// A turn can be assembling several calls at once.
    pub fn push(&mut self, delta: crate::backend::wire::ToolCallDelta) {
        if self.parts.len() <= delta.index {
            self.parts.resize(delta.index + 1, Part::default());
        }
        let part = &mut self.parts[delta.index];
        if let Some(id) = delta.id {
            part.id = Some(id);
        }
        if let Some(f) = delta.function {
            if let Some(name) = f.name {
                part.name.push_str(&name);
            }
            if let Some(args) = f.arguments {
                part.arguments.push_str(&args);
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    /// The calls as assembled, with their arguments still unparsed.
    ///
    /// A fragment index that was never filled leaves an empty name.
    /// Such a call is dropped rather than dispatched, since a call with no tool is not a call.
    pub fn finish(self) -> Vec<(Option<String>, String, String)> {
        self.parts
            .into_iter()
            .filter(|p| !p.name.is_empty())
            .map(|p| (p.id, p.name, p.arguments))
            .collect()
    }
}

/// The tool calls written as text inside a model's reasoning, in the order written.
///
/// 2 syntaxes are read:
/// - `<function=NAME>` with `<parameter=KEY>VALUE</parameter>` children;
/// - a JSON object with `name` and `arguments` between `<tool_call>` and `</tool_call>`.
///
/// `schema_of` gives the argument schema of a tool, and `None` for a tool that does not exist.
/// A call to a tool that does not exist is left out.
/// A parameter whose schema type is not `string` is read as JSON when it parses.
pub fn calls_in_reasoning(
    text: &str,
    schema_of: impl Fn(&str) -> Option<Value>,
) -> Vec<(String, Value)> {
    let mut calls = Vec::new();
    let mut rest = text;
    while let Some(start) = next_call(rest) {
        rest = &rest[start..];
        let (call, used) = if rest.starts_with(FUNCTION_OPEN) {
            function_call(rest, &schema_of)
        } else {
            json_call(rest)
        };
        if let Some((name, arguments)) = call {
            if schema_of(&name).is_some() {
                calls.push((name, arguments));
            }
        }
        rest = &rest[used.max(1)..];
    }
    calls
}

/// Whether `text` holds the opening of a tool call in one of the syntaxes a model writes.
pub fn looks_like_a_call(text: &str) -> bool {
    text.contains(FUNCTION_OPEN) || text.contains(TOOL_CALL_OPEN) || text.contains("\"tool_calls\"")
}

const FUNCTION_OPEN: &str = "<function=";
const TOOL_CALL_OPEN: &str = "<tool_call>";

/// Where the next call opens: a `<function=` tag, or a `<tool_call>` tag followed by JSON.
fn next_call(text: &str) -> Option<usize> {
    let function = text.find(FUNCTION_OPEN);
    let json = text.match_indices(TOOL_CALL_OPEN).find_map(|(i, tag)| {
        text[i + tag.len()..]
            .trim_start()
            .starts_with('{')
            .then_some(i)
    });
    match (function, json) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// One `<function=NAME>` call at the start of `text`, and the bytes it spans.
/// A missing closing tag ends the call at the end of the text.
fn function_call(
    text: &str,
    schema_of: &impl Fn(&str) -> Option<Value>,
) -> (Option<(String, Value)>, usize) {
    let Some(name_end) = text.find('>') else {
        return (None, text.len());
    };
    let name = text[FUNCTION_OPEN.len()..name_end].trim().to_string();
    let body_end = text[name_end..]
        .find("</function>")
        .map_or(text.len(), |i| name_end + i);
    let body = &text[name_end + 1..body_end];
    let used = (body_end + "</function>".len()).min(text.len());
    let schema = schema_of(&name).unwrap_or(Value::Null);
    let mut arguments = serde_json::Map::new();
    let mut params = body;
    while let Some(open) = params.find("<parameter=") {
        params = &params[open + "<parameter=".len()..];
        let Some(key_end) = params.find('>') else {
            break;
        };
        let key = params[..key_end].trim().to_string();
        params = &params[key_end + 1..];
        let value_end = params
            .find("</parameter>")
            .or_else(|| params.find("<parameter="))
            .unwrap_or(params.len());
        let raw = params[..value_end]
            .strip_prefix('\n')
            .unwrap_or(&params[..value_end]);
        let raw = raw.strip_suffix('\n').unwrap_or(raw);
        let typed = schema["properties"][&key]["type"].as_str() != Some("string");
        let value = typed
            .then(|| serde_json::from_str::<Value>(raw.trim()).ok())
            .flatten()
            .unwrap_or_else(|| Value::String(raw.to_string()));
        arguments.insert(key, value);
        params = &params[value_end..];
    }
    (Some((name, Value::Object(arguments))), used)
}

/// One `<tool_call>{"name": ..., "arguments": ...}</tool_call>` at the start of `text`, and the
/// bytes it spans.
/// Arguments sent as a string of JSON are parsed.
fn json_call(text: &str) -> (Option<(String, Value)>, usize) {
    let body_start = TOOL_CALL_OPEN.len();
    let body_end = text[body_start..]
        .find("</tool_call>")
        .map_or(text.len(), |i| body_start + i);
    let used = (body_end + "</tool_call>".len()).min(text.len());
    let Ok(v) = serde_json::from_str::<Value>(text[body_start..body_end].trim()) else {
        return (None, used);
    };
    let Some(name) = v["name"].as_str() else {
        return (None, used);
    };
    let arguments = match &v["arguments"] {
        Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null),
        Value::Null => Value::Object(Default::default()),
        other => other.clone(),
    };
    (Some((name.to_string(), arguments)), used)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::wire::{FunctionDelta, ToolCallDelta};

    fn schema_of(name: &str) -> Option<Value> {
        match name {
            "glob" => Some(serde_json::json!({"properties": {"pattern": {"type": "string"}}})),
            "read_file" => Some(serde_json::json!({"properties": {
                "path": {"type": "string"}, "start_line": {"type": "integer"}}})),
            _ => None,
        }
    }

    /// A call written in the `<function=` syntax inside reasoning is read with its arguments.
    /// A parameter typed as a number in the schema is read as one.
    #[test]
    fn a_function_call_in_reasoning_is_read() {
        let text = "Let me look first.\n\n<tool_call>\n<function=glob>\n<parameter=pattern>\n\
                    runs/**/*.json\n</parameter>\n</function>\n</tool_call>\n\
                    <tool_call>\n<function=read_file>\n<parameter=path>\na.rs\n</parameter>\n\
                    <parameter=start_line>\n12\n</parameter>\n</function>\n</tool_call>";
        let calls = calls_in_reasoning(text, schema_of);
        assert_eq!(
            calls,
            vec![
                (
                    "glob".to_string(),
                    serde_json::json!({"pattern": "runs/**/*.json"})
                ),
                (
                    "read_file".to_string(),
                    serde_json::json!({"path": "a.rs", "start_line": 12})
                ),
            ]
        );
    }

    /// A JSON call between `<tool_call>` tags is read, its arguments given as an object or as a
    /// string of JSON.
    #[test]
    fn a_json_call_in_reasoning_is_read() {
        let text = r#"<tool_call>{"name": "glob", "arguments": {"pattern": "*.rs"}}</tool_call>
            <tool_call>{"name": "read_file", "arguments": "{\"path\": \"b.rs\"}"}</tool_call>"#;
        let calls = calls_in_reasoning(text, schema_of);
        assert_eq!(
            calls,
            vec![
                ("glob".to_string(), serde_json::json!({"pattern": "*.rs"})),
                ("read_file".to_string(), serde_json::json!({"path": "b.rs"})),
            ]
        );
    }

    /// Prose that names a tag, and a call to a tool that does not exist, give no call.
    #[test]
    fn prose_and_unknown_tools_give_no_call() {
        assert!(calls_in_reasoning("I could use <tool_call> later.", schema_of).is_empty());
        assert!(calls_in_reasoning(
            "<function=delete_all>\n<parameter=path>\n/\n</parameter>\n</function>",
            schema_of
        )
        .is_empty());
    }

    fn frag(
        index: usize,
        id: Option<&str>,
        name: Option<&str>,
        args: Option<&str>,
    ) -> ToolCallDelta {
        ToolCallDelta {
            index,
            id: id.map(String::from),
            function: Some(FunctionDelta {
                name: name.map(String::from),
                arguments: args.map(String::from),
            }),
        }
    }

    /// Arguments arrive a few characters at a time.
    /// Nothing means anything until they are back together.
    #[test]
    fn fragments_of_one_call_reassemble_in_order() {
        let mut acc = CallAccumulator::default();
        acc.push(frag(0, Some("call_1"), Some("read_file"), Some("{\"pa")));
        acc.push(frag(0, None, None, Some("th\": \"a.rs\"")));
        acc.push(frag(0, None, None, Some("}")));

        let calls = acc.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0.as_deref(), Some("call_1"));
        assert_eq!(calls[0].1, "read_file");
        assert_eq!(calls[0].2, "{\"path\": \"a.rs\"}");
    }

    /// A turn can be assembling several calls at once, interleaved.
    #[test]
    fn two_calls_being_built_at_once_stay_apart() {
        let mut acc = CallAccumulator::default();
        acc.push(frag(0, Some("a"), Some("read_file"), Some("{\"path\":")));
        acc.push(frag(1, Some("b"), Some("grep"), Some("{\"pattern\":")));
        acc.push(frag(0, None, None, Some("\"one\"}")));
        acc.push(frag(1, None, None, Some("\"two\"}")));

        let calls = acc.finish();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, "read_file");
        assert_eq!(calls[0].2, "{\"path\":\"one\"}");
        assert_eq!(calls[1].1, "grep");
        assert_eq!(calls[1].2, "{\"pattern\":\"two\"}");
    }

    /// A server that numbers its calls from something other than zero must not make the accumulator
    /// index out of bounds.
    /// It must not produce a phantom call either.
    #[test]
    fn a_gap_in_the_indices_does_not_become_a_call() {
        let mut acc = CallAccumulator::default();
        acc.push(frag(2, Some("c"), Some("bash"), Some("{}")));

        let calls = acc.finish();
        assert_eq!(calls.len(), 1, "the unfilled slots became calls");
        assert_eq!(calls[0].1, "bash");
    }

    /// A name split across fragments is unusual but permitted by the format.
    #[test]
    fn a_split_name_is_joined() {
        let mut acc = CallAccumulator::default();
        acc.push(frag(0, Some("a"), Some("read_"), None));
        acc.push(frag(0, None, Some("file"), Some("{}")));
        assert_eq!(acc.finish()[0].1, "read_file");
    }

    #[test]
    fn nothing_in_means_nothing_out() {
        assert!(CallAccumulator::default().is_empty());
        assert!(CallAccumulator::default().finish().is_empty());
    }
}
