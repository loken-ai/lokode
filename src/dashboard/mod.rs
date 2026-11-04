//! Piloting the sessions of several lokode servers from one place.
//!
//! A server started with `lokode serve --share` announces itself to the machines of a group and
//! accepts their dashboards over an encrypted channel.
//! A group is a name and a secret, written under `[dashboard]` in the user's configuration.

pub mod discovery;
pub mod wire;
