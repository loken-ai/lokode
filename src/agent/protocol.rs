//! How a turn's tool calls travel, and the one shape the loop reads them in.
//!
//! Two ways of asking the same thing. The envelope makes the model emit a single JSON
//! object, constrained by a grammar built from the tool schemas; the native protocol uses
//! the server's own `tools` field and gets back `tool_calls`. Small local models are the
//! reason the envelope exists and the reason it stays: a server that will not do native
//! tool calls, or a model that does them badly, still works against a grammar.
//!
//! The loop sees neither. It gets a [`ModelTurn`], and everything downstream of that -
//! dispatch, approval, the transcript - is the same either way.

use serde::Deserialize;
use serde_json::Value;

/// Which way this session talks to the model about tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolProtocol {
    /// The server's `tools` field and `tool_calls` in the reply.
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
/// Which protocol works is a property of the backend, not of one session, and it is not
/// known until something has asked. A delegated turn or a background question started
/// after the answer came in has to speak what the main session settled on, or it does its
/// work with tools the server never passes on.
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
}

/// One call, with its arguments parsed and its identity settled.
#[derive(Debug, Clone)]
pub struct ResolvedCall {
    /// The id the result must be returned against.
    ///
    /// In the native protocol this is the server's own id, which is the point: a
    /// `role: "tool"` message answering an id nobody issued is a message a strict server
    /// rejects, and inventing one per result - as this used to - means every result
    /// answered a call that never existed.
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// The envelope protocol's per-turn object. Mirrors `build_turn_schema`.
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
    /// The index is the only thing tying fragments together: the name arrives once, the
    /// arguments arrive a few characters at a time, and a turn can be assembling several
    /// calls at once.
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
    /// A fragment index that was never filled leaves an empty name; those are dropped
    /// rather than dispatched, since a call with no tool is not a call.
    pub fn finish(self) -> Vec<(Option<String>, String, String)> {
        self.parts
            .into_iter()
            .filter(|p| !p.name.is_empty())
            .map(|p| (p.id, p.name, p.arguments))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::wire::{FunctionDelta, ToolCallDelta};

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

    /// Arguments arrive a few characters at a time; nothing means anything until they are
    /// back together.
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

    /// A server that numbers its calls from something other than zero must not make the
    /// accumulator index out of bounds, and must not produce a phantom call either.
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
