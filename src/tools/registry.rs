use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::sandbox::approval::{ApprovalSender, SessionApprovals};

#[derive(Clone)]
pub struct ToolCtx {
    pub root: PathBuf,
    pub approval: ApprovalSender,
    pub session_approvals: Arc<SessionApprovals>,
}

#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn ok(s: impl Into<String>) -> Self {
        Self { content: s.into(), is_error: false }
    }
    pub fn err(s: impl Into<String>) -> Self {
        Self { content: s.into(), is_error: true }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    /// JSON Schema for the `arguments` object the model must emit.
    fn args_schema(&self) -> Value;

    async fn run(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput>;
}

pub struct Registry {
    tools: HashMap<&'static str, Arc<dyn Tool>>,
}

impl Registry {
    pub fn new() -> Self {
        Self { tools: HashMap::new() }
    }
    pub fn register<T: Tool + 'static>(&mut self, tool: T) {
        self.tools.insert(tool.name(), Arc::new(tool));
    }
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }
    pub fn iter(&self) -> impl Iterator<Item = (&&'static str, &Arc<dyn Tool>)> {
        self.tools.iter()
    }
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}
