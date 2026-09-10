//! Tools from somewhere other than this repository.
//!
//! An MCP server is a program the user has configured, speaking a small protocol over its
//! standard input and output. Whatever it offers is adapted into the same `Tool` the rest of
//! the agent already knows, so the grammar, the manifest, the approval flow and the
//! checkpoints apply to it unchanged.
//!
//! What does not carry over is trust. A built-in tool is trusted because its behaviour is
//! here to read; one of these is a third party, so every call is approved, every call is
//! treated as able to change the project, and the schema it advertises is checked before it
//! is allowed to shape the grammar the model is held to.

pub mod client;
pub mod tool;

use std::sync::Arc;

use crate::config::McpServer;
use crate::tools::registry::{Registry, Tool};

/// Everything that came of starting the configured servers.
#[derive(Default)]
pub struct Loaded {
    pub clients: Vec<Arc<client::McpClient>>,
    /// The prompts each server offers: its name, the prompt's name, and what it is for.
    pub prompts: Vec<(String, String, String)>,
    /// Worth telling the user: a server that would not start, or a tool that was refused.
    pub notices: Vec<String>,
    pub tools: usize,
}

/// Start each configured server and register what it offers.
///
/// One server failing takes only that server with it. These are other people's programs and
/// a missing one is an ordinary thing to find; refusing to start the agent over it would be
/// a poor trade.
pub async fn load(
    servers: &std::collections::BTreeMap<String, McpServer>,
    registry: &mut Registry,
    root: &std::path::Path,
) -> Loaded {
    let mut out = Loaded::default();

    for (name, spec) in servers {
        if !spec.enabled {
            continue;
        }
        let client = match client::McpClient::start(
            name,
            &spec.command,
            &spec.args,
            &spec.env,
            root,
        )
        .await
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(server = %name, why = %format!("{e:#}"), "MCP server did not start");
                out.notices
                    .push(format!("MCP server {name} did not start: {e:#}"));
                continue;
            }
        };

        let offered = match client.list_tools().await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(server = %name, why = %format!("{e:#}"), "MCP server would not list its tools");
                out.notices
                    .push(format!("MCP server {name} would not list its tools: {e:#}"));
                continue;
            }
        };

        let mut adopted = 0;
        for info in &offered {
            match tool::McpTool::adopt(client.clone(), info) {
                Err(why) => {
                    tracing::warn!(server = %name, tool = %info.name, %why, "MCP tool refused");
                    out.notices
                        .push(format!("{name}/{}: refused because {why}", info.name));
                }
                Ok(t) => {
                    let tool_name = t.name().to_string();
                    if registry.add(Arc::new(t)) {
                        adopted += 1;
                    } else {
                        out.notices.push(format!(
                            "{name}/{}: {tool_name} is already taken",
                            info.name
                        ));
                    }
                }
            }
        }

        // A server's documents become one tool that reads them, so the approval flow,
        // the checkpoints and the transcript apply to them unchanged.
        let resources = client.list_resources().await;
        if !resources.is_empty() {
            let tool = tool::McpResources::new(client.clone(), &resources);
            if registry.add(Arc::new(tool)) {
                adopted += 1;
            }
        }
        let prompts = client.list_prompts().await;
        if !prompts.is_empty() {
            out.notices.push(format!(
                "MCP server {name} offers {} prompt(s): {}",
                prompts.len(),
                prompts
                    .iter()
                    .map(|(n, _)| n.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        out.prompts.extend(
            prompts
                .into_iter()
                .map(|(p, about)| (name.clone(), p, about)),
        );

        tracing::info!(
            server = %name,
            offered = offered.len(),
            adopted,
            resources = resources.len(),
            "MCP server ready"
        );
        out.tools += adopted;
        out.clients.push(client);
    }
    out
}
