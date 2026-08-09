use serde_json::{json, Value};

use crate::backend::wire::{FunctionSpec, ToolSpec};
use crate::tools::registry::Registry;

/// Builds the per-turn JSON Schema enforced server-side by `llguidance`.
///
/// Shape:
///
/// ```text
/// {
///   "assistant_text": "string",   // free-form prose to the user
///   "tool_calls": [               // 0..N tool calls executed in order
///     { "name": "<tool>", "arguments": <tool-specific schema> }
///   ]
/// }
/// ```
///
/// Per-tool argument schemas are unioned via `oneOf` keyed on `name`. The
/// `additionalProperties: false` + `required` constraints make the
/// grammar fail closed if the model invents a tool.
pub fn build_turn_schema(reg: &Registry) -> Value {
    let mut variants = Vec::with_capacity(reg.len());
    for (_, tool) in reg.iter() {
        variants.push(json!({
            "type": "object",
            "properties": {
                "name":      { "const": tool.name() },
                "arguments": tool.args_schema(),
            },
            "required": ["name", "arguments"],
            "additionalProperties": false
        }));
    }

    // Empty registry → at least allow text-only responses.
    let tool_call_schema = if variants.is_empty() {
        json!({ "type": "object" })
    } else {
        json!({ "oneOf": variants })
    };

    json!({
        "type": "object",
        "properties": {
            "assistant_text": { "type": "string" },
            "tool_calls": {
                "type":  "array",
                "items": tool_call_schema,
                "maxItems": 8
            }
        },
        "required": ["assistant_text", "tool_calls"],
        "additionalProperties": false
    })
}

/// The registry as the native protocol wants to see it.
///
/// The same per-tool schemas the envelope grammar is built from, carried in the field the
/// server understands instead of described in the preamble. Sorted, because a stable
/// order is a stable prompt prefix, and the server reuses its KV cache on one.
pub fn tool_specs(reg: &Registry) -> Vec<ToolSpec> {
    let mut tools: Vec<_> = reg.iter().collect();
    tools.sort_by(|a, b| a.0.cmp(b.0));
    tools
        .into_iter()
        .map(|(_, tool)| ToolSpec {
            kind: "function".into(),
            function: FunctionSpec {
                name: tool.name().into(),
                description: tool.description().into(),
                parameters: tool.args_schema(),
            },
        })
        .collect()
}

/// Description block prepended to the system prompt so the model knows
/// which tools exist and what each does. The schema is the source of
/// truth for structure; this is for semantic guidance only.
pub fn tool_manifest(reg: &Registry) -> String {
    let mut out = String::from("Tools available:\n");
    let mut entries: Vec<_> = reg.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    for (_, tool) in entries {
        out.push_str(&format!("- {}: {}\n", tool.name(), tool.description()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::fs_read::ListDir;

    #[test]
    fn schema_includes_registered_tool() {
        let mut reg = Registry::new();
        reg.register(ListDir);
        let schema = build_turn_schema(&reg);
        let one_of = schema["properties"]["tool_calls"]["items"]["oneOf"]
            .as_array()
            .unwrap();
        assert_eq!(one_of.len(), 1);
        assert_eq!(one_of[0]["properties"]["name"]["const"], "list_dir");
    }
}
