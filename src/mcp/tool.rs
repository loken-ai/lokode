//! An MCP server's tool, made to look like any other.
//!
//! The registry does not know the difference, which is the point: the grammar, the manifest,
//! the approval flow and the checkpoint all work on it unchanged. What is different is where
//! the schema came from. Every other tool's schema is written here and can be trusted to
//! build a grammar out of; this one arrives from a program somebody else wrote, and a schema
//! the grammar builder cannot handle would not break one tool - it would break the whole
//! turn, for every tool.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::mcp::client::{McpClient, McpToolInfo};
use crate::sandbox::approval::{Approval, ApprovalRequest, ApprovalScope};
use crate::tools::registry::{Tool, ToolCtx, ToolOutput};

/// How deep a supplied schema may nest before it is refused.
const MAX_DEPTH: usize = 12;

pub struct McpTool {
    /// `<server>__<tool>`, so two servers offering `search` do not collide and neither can
    /// take the name of a built-in.
    name: String,
    description: String,
    schema: Value,
    client: Arc<McpClient>,
    remote_name: String,
}

impl McpTool {
    /// Adapt one advertised tool, or say why it cannot be.
    pub fn adopt(client: Arc<McpClient>, info: &McpToolInfo) -> Result<Self, String> {
        let schema = usable_schema(&info.input_schema)?;
        Ok(Self {
            name: format!("{}__{}", client.server, info.name),
            description: if info.description.trim().is_empty() {
                format!("(from the {} MCP server)", client.server)
            } else {
                format!("{} (via {})", info.description.trim(), client.server)
            },
            schema,
            remote_name: info.name.clone(),
            client,
        })
    }
}

/// Check a supplied schema, and normalise what is merely missing.
///
/// The refusals are the ones that would poison the grammar rather than merely produce a bad
/// call: a reference this code cannot resolve, or a structure deep enough to suggest it was
/// not written for this. Anything else is left as the server wrote it - being strict about
/// style here would reject working servers for no gain.
fn usable_schema(raw: &Value) -> Result<Value, String> {
    // A tool that takes nothing may say so by saying nothing.
    if raw.is_null() {
        return Ok(json!({"type": "object", "properties": {}, "additionalProperties": false}));
    }
    let Some(obj) = raw.as_object() else {
        return Err("its input schema is not an object".into());
    };
    match obj.get("type").and_then(|t| t.as_str()) {
        Some("object") | None => {}
        Some(other) => return Err(format!("its input schema is a {other}, not an object")),
    }
    if let Some(path) = find_reference(raw, 0)? {
        return Err(format!(
            "its input schema uses {path}, which cannot be resolved here"
        ));
    }

    let mut schema = raw.clone();
    // The grammar is built from this, and a schema with no `type` produces one that
    // constrains nothing.
    if let Some(map) = schema.as_object_mut() {
        map.entry("type").or_insert_with(|| json!("object"));
        map.entry("properties").or_insert_with(|| json!({}));
    }
    Ok(schema)
}

/// Look for a reference the schema compiler here cannot follow, and for runaway nesting.
fn find_reference(v: &Value, depth: usize) -> Result<Option<String>, String> {
    if depth > MAX_DEPTH {
        return Err(format!("its input schema nests more than {MAX_DEPTH} deep"));
    }
    match v {
        Value::Object(map) => {
            for key in ["$ref", "$dynamicRef", "$recursiveRef"] {
                if map.contains_key(key) {
                    return Ok(Some(key.to_string()));
                }
            }
            for value in map.values() {
                if let Some(found) = find_reference(value, depth + 1)? {
                    return Ok(Some(found));
                }
            }
            Ok(None)
        }
        Value::Array(items) => {
            for item in items {
                if let Some(found) = find_reference(item, depth + 1)? {
                    return Ok(Some(found));
                }
            }
            Ok(None)
        }
        _ => Ok(None),
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn args_schema(&self) -> Value {
        self.schema.clone()
    }

    /// Assumed, because it cannot be known.
    ///
    /// A server describes what its tool is called and what it takes, and says nothing about
    /// whether running it changes the project. Treating it as mutating costs a checkpoint
    /// that may turn out to record nothing; treating it as pure would mean a change nobody
    /// can undo.
    fn mutates(&self) -> bool {
        true
    }

    async fn run(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput> {
        // Gated without exception. A built-in tool is trusted because its behaviour is in
        // this repository; this one is a program the user configured, and the root does not
        // confine it any more than it confines a shell.
        match ctx
            .approve(ApprovalRequest {
                tool: self.name.clone(),
                summary: format!(
                    "{} via the {} MCP server\n{}",
                    self.remote_name,
                    self.client.server,
                    serde_json::to_string_pretty(&args).unwrap_or_default()
                ),
                diff_preview: None,
                // Per tool rather than per server: agreeing to one of a server's tools is
                // not agreeing to the rest of them.
                scope: ApprovalScope::Tool(self.name.clone()),
            })
            .await
        {
            Approval::Proceed => {}
            Approval::Denied => return Ok(ToolOutput::err("user denied")),
            Approval::Interrupted => return Ok(ToolOutput::err("interrupted by user")),
        }

        let call = self.client.call_tool(&self.remote_name, args);
        let (text, is_error) = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => return Ok(ToolOutput::err("interrupted by user")),
            r = call => match r {
                Ok(v) => v,
                Err(e) => return Ok(ToolOutput::err(format!("{}: {e:#}", self.client.server))),
            },
        };

        let text = ctx.clamp(text);
        Ok(if is_error {
            ToolOutput::err(text)
        } else {
            ToolOutput::ok(text)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tool_that_takes_nothing_still_gets_a_schema() {
        let schema = usable_schema(&Value::Null).unwrap();
        assert_eq!(schema["type"], "object");
    }

    #[test]
    fn a_missing_type_is_filled_in_rather_than_refused() {
        let schema = usable_schema(&json!({"properties": {"q": {"type": "string"}}})).unwrap();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["q"]["type"], "string");
    }

    #[test]
    fn an_ordinary_schema_is_passed_through_as_written() {
        let raw = json!({
            "type": "object",
            "properties": {"q": {"type": "string"}, "n": {"type": "integer"}},
            "required": ["q"]
        });
        assert_eq!(usable_schema(&raw).unwrap(), raw);
    }

    /// The refusal that matters: the grammar is built from every tool's schema at once, so
    /// one this code cannot compile does not break one tool - it breaks the turn.
    #[test]
    fn a_schema_with_a_reference_is_refused() {
        let err = usable_schema(&json!({
            "type": "object",
            "properties": {"x": {"$ref": "#/definitions/Thing"}}
        }))
        .unwrap_err();
        assert!(err.contains("$ref"), "unexpected: {err}");
    }

    #[test]
    fn a_schema_that_is_not_an_object_is_refused() {
        assert!(usable_schema(&json!("a string")).is_err());
        assert!(usable_schema(&json!({"type": "array"})).is_err());
    }

    #[test]
    fn a_runaway_schema_is_refused() {
        let mut deep = json!({"type": "string"});
        for _ in 0..40 {
            deep = json!({"type": "object", "properties": {"nested": deep}});
        }
        let err = usable_schema(&deep).unwrap_err();
        assert!(err.contains("deep"), "unexpected: {err}");
    }
}
