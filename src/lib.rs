//! A coding agent that runs in your terminal against a model on your own machine.
//!
//! The binary is the product.
//! This library is what it is assembled from, and the headless example drives the same pieces.
//!
//! # How a turn works
//!
//! [`ui`] owns the screen.
//! It hands a user's input to a task holding an [`agent::agent_loop::Agent`].
//! The task also holds a cancellation token, so `Esc` reaches the turn rather than waiting for it.
//!
//! The agent asks the model.
//! It reads the answer as an [`agent::protocol::ModelTurn`], whichever protocol produced it.
//! It runs the tools the turn names and feeds the results back.
//! It repeats until the model stops asking for tools or `--max-steps` runs out.
//!
//! # The parts
//!
//! - [`acp`]: lokode as the agent of an editor, over the Agent Client Protocol.
//! - [`agent`]: the loop, the conversation and what it is written to.
//! - [`backend`]: the providers, the wire types, and the repair pass for the JSON a model gets
//!   slightly wrong.
//!   [`backend::provider::Provider`] is the seam.
//!   Everything about whose API is answering lives behind it.
//! - [`tools`]: the things the agent can do, and the registry they are named in.
//! - [`sandbox`]: what the agent may touch, and what the user has agreed to.
//! - [`config`]: settings, from a flag, the environment, a project file or a user file.
//! - [`paths`]: where logs and transcripts live.
//! - [`serve`]: the sessions of a project in a browser page, over the same protocol.
//!
//! # 2 things worth knowing before changing anything
//!
//! The system message carries the tool manifest.
//! A server that has to truncate a too-long conversation keeps the tail.
//! So letting the history overflow the window removes the agent's knowledge of its own tools.
//! [`agent::session::Session`] therefore compacts client-side.
//! A single tool result is sized against the window it has to fit in.
//!
//! The path sandbox in [`sandbox::allowlist`] confines every tool that takes a path.
//! A shell takes a string, so [`sandbox::confine`] confines the commands themselves.
//! It uses Landlock on Linux and Seatbelt on macOS, unless it is turned off.
//! See `SECURITY.md`.

pub mod acp;
pub mod agent;
pub mod app;
pub mod backend;
pub mod boot;
pub mod checkpoint;
pub mod claude;
pub mod commands;
pub mod config;
pub mod frontmatter;
pub mod headless;
pub mod lsp;
pub mod marketplace;
pub mod mcp;
pub mod paths;
pub mod plugins;
pub mod sandbox;
pub mod serve;
pub mod shell;
pub mod sound;
pub mod tools;
pub mod trust;
pub mod ui;
