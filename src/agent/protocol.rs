//! On-the-wire envelope the model emits each turn. Mirrors `build_turn_schema`.

use serde::Deserialize;
use serde_json::Value;

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
