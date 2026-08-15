//! A Model Context Protocol server for the crates.io registry.
//!
//! Exposes five read-only tools — `search_crates`, `get_crate_info`,
//! `get_crate_versions`, `get_crate_dependencies` and
//! `get_crate_documentation` — over the transport an MCP client provides.
//!
//! All registry access, pacing and caching lives in the `crates-io-client`
//! crate; this crate is the protocol surface over it.

pub mod error;
pub mod server;
pub mod tools;
pub mod untrusted;

pub use crate::server::CratesServer;
