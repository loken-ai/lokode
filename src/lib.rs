//! A coding agent that runs in your terminal against a model on your own machine.
//!
//! The binary is the product; this library is what it is assembled from, exposed because the
//! headless example drives the same pieces.
//!
//! # How a turn works
//!
//! [`ui`] owns the screen and hands a user's input to a task holding an [`agent::agent_loop::Agent`],
//! together with a cancellation token it keeps - so `Esc` reaches the turn rather than waiting
//! for it. The agent asks the model, reads the answer as an [`agent::protocol::ModelTurn`]
//! whichever protocol produced it, runs the tools it names, feeds the results back, and repeats
//! until the model stops asking for tools or `--max-steps` runs out.
//!
//! # The parts
//!
//! - [`agent`] - the loop, the conversation and what it is written to.
//! - [`backend`] - an OpenAI-compatible client, the wire types, and the repair pass for the
//!   JSON a model gets slightly wrong.
//! - [`tools`] - the twelve things the agent can do, and the registry they are named in.
//! - [`sandbox`] - what the agent may touch, and what the user has agreed to.
//! - [`config`] - settings, from a flag, the environment, a project file or a user file.
//! - [`paths`] - where logs and transcripts live.
//!
//! # Two things worth knowing before changing anything
//!
//! The system message carries the tool manifest, and a server that has to truncate a
//! too-long conversation keeps the tail. So letting the history overflow the window removes
//! the agent's knowledge of its own tools, which is why [`agent::session::Session`] compacts
//! client-side and why a single tool result is sized against the window it has to fit in.
//!
//! The path sandbox in [`sandbox::allowlist`] confines every tool that takes a path. It does
//! not confine `bash`, which by construction runs a shell. See `SECURITY.md`.

pub mod agent;
pub mod app;
pub mod backend;
pub mod boot;
pub mod config;
pub mod headless;
pub mod paths;
pub mod sandbox;
pub mod tools;
pub mod ui;
