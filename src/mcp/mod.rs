//! Tools from somewhere other than this repository.
//!
//! An MCP server is a program the user has configured, speaking a small protocol over its
//! standard input and output.
//! Whatever it offers is adapted into the same `Tool` the rest of the agent already knows.
//! The grammar, the manifest, the approval flow and the checkpoints apply to it unchanged.
//!
//! Trust does not carry over.
//! A built-in tool is trusted because its behaviour is here to read.
//! A server is a third party, so every call is approved.
//! Every call is treated as able to change the project.
//! The schema a server advertises is checked before it may shape the grammar the model is held to.

pub mod client;
pub mod host;
pub mod oauth;
pub mod tool;

use std::sync::Arc;

use crate::config::McpServer;
use crate::tools::registry::{Registry, Tool};

/// The servers of a session, and what answers their questions.
#[derive(Default)]
pub struct Servers {
    list: std::sync::RwLock<Vec<Arc<client::McpClient>>>,
    host: host::SharedHost,
}

/// The servers, shared by the agent and the interface.
/// A server started again is the one both speak to.
pub type Clients = Arc<Servers>;

impl Servers {
    pub fn new(list: Vec<Arc<client::McpClient>>, host: host::SharedHost) -> Clients {
        Arc::new(Self {
            list: std::sync::RwLock::new(list),
            host,
        })
    }
}

/// The servers as they stand, without holding the list.
pub fn snapshot(clients: &Clients) -> Vec<Arc<client::McpClient>> {
    clients.list.read().map(|c| c.clone()).unwrap_or_default()
}

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

/// Register the tools `offered` by `client`, and say which could not be.
pub fn adopt(
    client: &Arc<client::McpClient>,
    offered: &[client::McpToolInfo],
    registry: &mut Registry,
) -> (usize, Vec<String>) {
    let name = &client.server;
    let mut adopted = 0;
    let mut notices = Vec::new();
    for info in offered {
        match tool::McpTool::adopt(client.clone(), info) {
            Err(why) => {
                tracing::warn!(server = %name, tool = %info.name, %why, "MCP tool refused");
                notices.push(format!("{name}/{}: refused because {why}", info.name));
            }
            Ok(t) => {
                let tool_name = t.name().to_string();
                if registry.add(Arc::new(t)) {
                    adopted += 1;
                } else {
                    notices.push(format!(
                        "{name}/{}: {tool_name} is already taken",
                        info.name
                    ));
                }
            }
        }
    }
    (adopted, notices)
}

/// Take the tools of `client` again, after it said they changed.
/// Tools it no longer offers leave `registry`, and the new ones join it.
/// Says how many it has now.
pub async fn readopt(
    client: &Arc<client::McpClient>,
    registry: &mut Registry,
) -> anyhow::Result<(usize, Vec<String>)> {
    let offered = client.list_tools().await?;
    let prefix = format!("{}__", client.server);
    let resources = tool::McpResources::tool_name(&client.server);
    registry.retain(|name| !name.starts_with(&prefix) || name == resources);
    let (adopted, notices) = adopt(client, &offered, registry);
    client.set_adopted(adopted);
    Ok((adopted, notices))
}

/// Start the server of `old` again, as it was configured.
/// It becomes the one `clients` and `registry` hold, and its tools and documents are taken again.
/// Says what it has.
pub async fn restart(
    old: &Arc<client::McpClient>,
    clients: &Clients,
    registry: &mut Registry,
) -> anyhow::Result<String> {
    let (spec, root) =
        old.started_from.get().cloned().ok_or_else(|| {
            anyhow::anyhow!("{} was not started from a configuration", old.server)
        })?;
    old.shutdown().await;
    let new = start(&old.server, &spec, &root, old.host()).await?;
    take_in(new, clients, registry).await
}

/// Start a server the session does not have yet, `name` configured as `spec`.
/// Its tools are taken.
/// Says what it has.
pub async fn add(
    name: &str,
    spec: &McpServer,
    root: &std::path::Path,
    clients: &Clients,
    registry: &mut Registry,
) -> anyhow::Result<String> {
    let new = start(name, spec, root, clients.host.clone()).await?;
    take_in(new, clients, registry).await
}

/// Make `new` the server of its name that `clients` and `registry` hold.
/// Its tools and documents are taken again.
async fn take_in(
    new: Arc<client::McpClient>,
    clients: &Clients,
    registry: &mut Registry,
) -> anyhow::Result<String> {
    let prefix = format!("{}__", new.server);
    registry.retain(|name| !name.starts_with(&prefix));
    let offered = new.list_tools().await?;
    let (mut adopted, notices) = adopt(&new, &offered, registry);
    let resources = new.list_resources().await;
    if !resources.is_empty()
        && registry.add(Arc::new(tool::McpResources::new(new.clone(), &resources)))
    {
        adopted += 1;
    }
    new.set_adopted(adopted);
    if let Ok(mut list) = clients.list.write() {
        match list.iter_mut().find(|c| c.server == new.server) {
            Some(slot) => *slot = new.clone(),
            None => list.push(new.clone()),
        }
    }
    let mut said = format!("{} runs with {adopted} tool(s)", new.server);
    for notice in notices {
        said.push_str(&format!("; {notice}"));
    }
    Ok(said)
}

/// Start one server, or reach it over HTTP.
async fn start(
    name: &str,
    spec: &McpServer,
    root: &std::path::Path,
    host: host::SharedHost,
) -> anyhow::Result<Arc<client::McpClient>> {
    let timeout = spec
        .timeout
        .map(std::time::Duration::from_secs)
        .unwrap_or(client::TIMEOUT);
    let client = match (&spec.command, &spec.url) {
        (Some(command), None) => {
            client::McpClient::start(name, command, &spec.args, &spec.env, root, timeout, host)
                .await
        }
        (None, Some(url)) => {
            let mut headers: Vec<(String, String)> = spec.headers.clone().into_iter().collect();
            if let Some(var) = &spec.token_env {
                let token = std::env::var(var)
                    .map_err(|_| anyhow::anyhow!("the variable {var} holds no token"))?;
                headers.push(("Authorization".into(), format!("Bearer {token}")));
            }
            client::McpClient::connect(name, url, headers, root, timeout, host).await
        }
        _ => anyhow::bail!("a server needs either a command or a url"),
    }?;
    let _ = client.started_from.set((spec.clone(), root.to_path_buf()));
    Ok(client)
}

/// Start each configured server and register what it offers.
///
/// One server failing takes only that server with it.
/// A missing server is an ordinary thing to find, so it never stops the agent from starting.
pub async fn load(
    servers: &std::collections::BTreeMap<String, McpServer>,
    registry: &mut Registry,
    root: &std::path::Path,
    host: &host::SharedHost,
) -> Loaded {
    let mut out = Loaded::default();

    // Started side by side, and adopted in the order of their names.
    let enabled: Vec<_> = servers.iter().filter(|(_, s)| s.enabled).collect();
    let started = futures::future::join_all(
        enabled
            .iter()
            .map(|(name, spec)| start(name, spec, root, host.clone())),
    )
    .await;
    for ((name, _), started) in enabled.into_iter().zip(started) {
        let client = match started {
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

        let (mut adopted, notices) = adopt(&client, &offered, registry);
        out.notices.extend(notices);
        // A server's documents become one tool that reads them.
        // The approval flow, the checkpoints and the transcript apply to it unchanged.
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
        client.set_adopted(adopted);
        out.clients.push(client);
    }
    out
}
