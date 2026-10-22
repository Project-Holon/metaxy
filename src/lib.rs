//! metaxy: the relay server for eidos multiplayer sessions.
//!
//! The protocol is in `docs/SPEC.md`.

#![forbid(unsafe_code)]

pub mod codec;
pub mod config;
pub mod outbox;
pub mod registry;
pub mod server;

pub use config::Config;
pub use server::Server;
