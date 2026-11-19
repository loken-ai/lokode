//! What a server may ask of the session: a word from the model, or values from the person.
//!
//! A server asks the model through `sampling/createMessage`.
//! The person is asked first, as for a tool call.
//! The session's own model answers, never one the server names.
//!
//! A server asks the person through `elicitation/create`, with a message and a flat form of
//! named fields.
//! The interface asks each field in turn.
//! A session with nobody to ask declines.
//!
//! The servers start before the backend is known.
//! What answers them is set once the session is assembled.
//! A request before that is refused.

use std::sync::{Arc, OnceLock};

use serde_json::{json, Map, Value};
use tokio::sync::{mpsc, oneshot};

use crate::backend::provider::{Provider, TurnRequest};
use crate::backend::wire::Message;
use crate::sandbox::approval::{
    self, ApprovalDecision, ApprovalRequest, ApprovalScope, ApprovalSender, SessionApprovals,
};

/// Characters of a server's request shown in the approval line.
const SHOWN_CHARS: usize = 160;

/// What answers the servers' requests.
pub struct Host {
    pub approval: ApprovalSender,
    pub session_approvals: Arc<SessionApprovals>,
    pub backend: Arc<dyn Provider>,
    pub model: String,
    /// Tokens an answer may take at most, whatever the server asks.
    pub max_tokens: u32,
    /// Where questions to the person go. `None` when nobody is there to answer.
    pub questions: Option<mpsc::Sender<Question>>,
}

/// The host, shared by every server of a session, set once the session is assembled.
pub type SharedHost = Arc<OnceLock<Host>>;

/// An error for the server: a JSON-RPC code and a message.
pub type Refusal = (i64, String);

/// The code for a request the client turns down.
const DECLINED: i64 = -1;

/// The code for a request the client cannot read.
const INVALID: i64 = -32602;

/// What kind of value a field takes.
#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Text,
    Number,
    Integer,
    Bool,
    /// One of these values.
    Choice(Vec<String>),
}

/// One field of a server's form.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub name: String,
    /// What the server calls it for the person, its name when it says nothing.
    pub title: String,
    pub description: String,
    pub kind: Kind,
    pub required: bool,
    pub default: Option<Value>,
}

impl Field {
    /// How the field is put to the person: its title, what it takes, and what it is for.
    pub fn prompt(&self) -> String {
        let takes = match &self.kind {
            Kind::Text => "text".to_string(),
            Kind::Number => "a number".to_string(),
            Kind::Integer => "a whole number".to_string(),
            Kind::Bool => "yes or no".to_string(),
            Kind::Choice(values) => format!("one of {}", values.join(", ")),
        };
        let mut out = format!("{} ({takes}", self.title);
        if !self.required {
            out.push_str(", may be left empty");
        }
        if let Some(default) = &self.default {
            out.push_str(&format!(", {default} when empty"));
        }
        out.push(')');
        if !self.description.is_empty() {
            out.push_str(&format!(": {}", self.description));
        }
        out
    }

    /// The value `typed` gives this field.
    /// `None` for nothing typed in a field that may be left empty, or why it does not fit.
    pub fn read(&self, typed: &str) -> Result<Option<Value>, String> {
        let typed = typed.trim();
        if typed.is_empty() {
            return match (&self.default, self.required) {
                (Some(d), _) => Ok(Some(d.clone())),
                (None, false) => Ok(None),
                (None, true) => Err(format!("{} must be given", self.title)),
            };
        }
        match &self.kind {
            Kind::Text => Ok(Some(json!(typed))),
            Kind::Number => typed
                .parse::<f64>()
                .map(|n| Some(json!(n)))
                .map_err(|_| format!("{typed} is not a number")),
            Kind::Integer => typed
                .parse::<i64>()
                .map(|n| Some(json!(n)))
                .map_err(|_| format!("{typed} is not a whole number")),
            Kind::Bool => match typed.to_lowercase().as_str() {
                "y" | "yes" | "true" | "1" => Ok(Some(json!(true))),
                "n" | "no" | "false" | "0" => Ok(Some(json!(false))),
                _ => Err(format!("{typed} is neither yes nor no")),
            },
            Kind::Choice(values) => values
                .iter()
                .find(|v| v.eq_ignore_ascii_case(typed))
                .map(|v| Some(json!(v)))
                .ok_or_else(|| format!("{typed} is not one of {}", values.join(", "))),
        }
    }
}

/// How the person answered a form.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    Accept(Map<String, Value>),
    Decline,
}

/// A server's form, put to the person.
#[derive(Debug)]
pub struct Question {
    pub server: String,
    pub message: String,
    pub fields: Vec<Field>,
    pub reply: oneshot::Sender<Answer>,
}

/// The fields of a requested schema.
/// It is an object whose properties are each a string, a number, an integer, a boolean or
/// a list of choices.
pub fn fields(schema: &Value) -> Result<Vec<Field>, String> {
    let required: Vec<&str> = schema
        .get("required")
        .and_then(|r| r.as_array())
        .into_iter()
        .flatten()
        .filter_map(|r| r.as_str())
        .collect();
    let Some(properties) = schema.get("properties").and_then(|p| p.as_object()) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for (name, p) in properties {
        let text = |key: &str| p.get(key).and_then(|v| v.as_str()).map(str::to_string);
        let choices: Option<Vec<String>> = p.get("enum").and_then(|e| e.as_array()).map(|e| {
            e.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        });
        let kind = match (text("type").as_deref(), choices) {
            (_, Some(values)) if !values.is_empty() => Kind::Choice(values),
            (Some("string"), _) => Kind::Text,
            (Some("number"), _) => Kind::Number,
            (Some("integer"), _) => Kind::Integer,
            (Some("boolean"), _) => Kind::Bool,
            (other, _) => return Err(format!("the field {name} has a type {other:?}")),
        };
        out.push(Field {
            title: text("title").unwrap_or_else(|| name.clone()),
            description: text("description").unwrap_or_default(),
            required: required.contains(&name.as_str()),
            default: p.get("default").cloned(),
            name: name.clone(),
            kind,
        });
    }
    // The map does not keep the server's order, so the required fields are asked first.
    out.sort_by_key(|f| !f.required);
    Ok(out)
}

/// The schema `fields` were read from, for handing the form on to another client.
pub fn schema(fields: &[Field]) -> Value {
    let mut properties = Map::new();
    for f in fields {
        let mut p = Map::new();
        let kind = match &f.kind {
            Kind::Text | Kind::Choice(_) => "string",
            Kind::Number => "number",
            Kind::Integer => "integer",
            Kind::Bool => "boolean",
        };
        p.insert("type".into(), json!(kind));
        if let Kind::Choice(values) = &f.kind {
            p.insert("enum".into(), json!(values));
        }
        p.insert("title".into(), json!(f.title));
        if !f.description.is_empty() {
            p.insert("description".into(), json!(f.description));
        }
        if let Some(d) = &f.default {
            p.insert("default".into(), d.clone());
        }
        properties.insert(f.name.clone(), Value::Object(p));
    }
    let required: Vec<&str> = fields
        .iter()
        .filter(|f| f.required)
        .map(|f| f.name.as_str())
        .collect();
    json!({"type": "object", "properties": properties, "required": required})
}

impl Host {
    /// Put a server's form to the person, and say how they answered.
    pub async fn elicit(&self, server: &str, params: &Value) -> Result<Value, Refusal> {
        let message = params
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or_default()
            .to_string();
        let fields = fields(params.get("requestedSchema").unwrap_or(&Value::Null))
            .map_err(|why| (INVALID, why))?;
        let Some(questions) = &self.questions else {
            return Ok(json!({"action": "decline"}));
        };
        let (tx, rx) = oneshot::channel();
        let asked = Question {
            server: server.to_string(),
            message,
            fields,
            reply: tx,
        };
        if questions.send(asked).await.is_err() {
            return Ok(json!({"action": "cancel"}));
        }
        Ok(match rx.await {
            Ok(Answer::Accept(content)) => json!({"action": "accept", "content": content}),
            Ok(Answer::Decline) => json!({"action": "decline"}),
            Err(_) => json!({"action": "cancel"}),
        })
    }

    /// Ask the session's model what a server wants asked, once the person agrees.
    pub async fn sample(&self, server: &str, params: &Value) -> Result<Value, Refusal> {
        let mut messages = Vec::new();
        if let Some(system) = params.get("systemPrompt").and_then(|s| s.as_str()) {
            messages.push(Message::system(system.to_string()));
        }
        for m in params
            .get("messages")
            .and_then(|m| m.as_array())
            .into_iter()
            .flatten()
        {
            let text = m
                .pointer("/content/text")
                .and_then(|t| t.as_str())
                .ok_or((INVALID, "lokode reads text messages only".to_string()))?;
            messages.push(match m.get("role").and_then(|r| r.as_str()) {
                Some("assistant") => Message::assistant(text.to_string()),
                _ => Message::user(text.to_string()),
            });
        }
        let last = messages
            .iter()
            .rev()
            .find(|m| m.role == "user")
            .ok_or((INVALID, "the request asks the model nothing".to_string()))?
            .content
            .clone();
        let max_tokens = params
            .get("maxTokens")
            .and_then(|m| m.as_u64())
            .map(|m| (m as u32).min(self.max_tokens))
            .unwrap_or(self.max_tokens);

        let scope = ApprovalScope::Tool(format!("{server} sampling"));
        if !self.session_approvals.is_always(&scope).await {
            let shown: String = last.chars().take(SHOWN_CHARS).collect();
            let req = ApprovalRequest {
                tool: format!("{server} sampling"),
                summary: format!("{server} asks the model ({max_tokens} tokens at most): {shown}"),
                diff_preview: Some(
                    messages
                        .iter()
                        .map(|m| format!("{}: {}", m.role, m.content))
                        .collect::<Vec<_>>()
                        .join("\n\n"),
                ),
                scope: scope.clone(),
            };
            match approval::ask(&self.approval, req).await {
                ApprovalDecision::Once => {}
                ApprovalDecision::Always => self.session_approvals.mark_always(scope).await,
                ApprovalDecision::Deny(why) => {
                    return Err((DECLINED, approval::denial(approval::Denial::User(why))))
                }
            }
        }

        let temperature = params
            .get("temperature")
            .and_then(|t| t.as_f64())
            .map(|t| t as f32);
        let text = self
            .backend
            .complete(TurnRequest {
                model: self.model.clone(),
                messages,
                tools: None,
                grammar: None,
                max_tokens,
                temperature,
                user: None,
                reason: false,
                purpose: crate::backend::meter::Purpose::Sampling,
            })
            .await
            .map_err(|e| (DECLINED, format!("the model did not answer: {e:#}")))?;
        Ok(json!({
            "role": "assistant",
            "content": {"type": "text", "text": text},
            "model": self.model,
            "stopReason": "endTurn",
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A schema's fields are read with their kind, and what is typed is held to it.
    #[test]
    fn a_form_is_read_and_its_answers_checked() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "title": "Your name"},
                "age": {"type": "integer"},
                "ok": {"type": "boolean", "default": true},
                "colour": {"type": "string", "enum": ["red", "green"]}
            },
            "required": ["name", "age"]
        });
        let fields = fields(&schema).unwrap();
        let by = |n: &str| fields.iter().find(|f| f.name == n).unwrap();
        assert_eq!(by("name").title, "Your name");
        assert!(by("name").required && !by("colour").required);
        assert_eq!(by("age").read("41"), Ok(Some(json!(41))));
        assert!(by("age").read("old").is_err());
        assert!(by("name").read("").is_err());
        assert_eq!(by("ok").read(""), Ok(Some(json!(true))));
        assert_eq!(by("ok").read("no"), Ok(Some(json!(false))));
        assert_eq!(by("colour").read("GREEN"), Ok(Some(json!("green"))));
        assert_eq!(by("colour").read(""), Ok(None));
        assert!(by("colour").prompt().contains("one of red, green"));
        assert!(fields_err(json!({"properties": {"x": {"type": "object"}}})));
    }

    fn fields_err(schema: Value) -> bool {
        fields(&schema).is_err()
    }
}
