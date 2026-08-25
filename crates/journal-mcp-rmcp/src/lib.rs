#![warn(missing_docs)]

//! # journal-mcp-rmcp
//!
//! ## Architecture
//!
//! `journal-mcp-rmcp` is the MCP transport layer for journal-mcp: it wires
//! `journal-mcp-core`'s `JournalCore` onto the Model Context Protocol via
//! the `rmcp` crate (stdio transport by default, streamable HTTP daemon via
//! [`run_http`], `#[tool_router]` dispatch for the 18 journal tools). It has
//! no `main` of its own; the `journal-mcp` binary crate is a thin wrapper
//! that constructs [`RunConfig`] from env vars / flags and calls [`run`]
//! (stdio) or [`run_http`] (`--mcp-http`).
//!
//! ## Design
//!
//! - `server`: [`JournalMcpServer`] — holds the `JournalCore` behind
//!   `Arc<Mutex<..>>`, an `extra_cores` per-project cache, and startup
//!   metadata. Implements `ServerHandler`.
//! - `tools`: the 18 `#[tool]`-annotated MCP tool handlers (chapter CRUD,
//!   append/section/progress, schema, tail/grep/list/dump, projection,
//!   import, info). Registered in a single `#[tool_router]` block (Crux #1).
//! - `request`: MCP request DTOs (`schemars::JsonSchema` + `serde`) —
//!   one Params struct per tool.
//!
//! Consumers that only need to run the server as-is should call [`run`]
//! with a [`RunConfig`]. Consumers that want to embed the server directly
//! (e.g. as part of a larger MCP host like lds) can construct
//! [`JournalMcpServer`] and drive it with any `rmcp` transport.

mod request;
mod server;
mod tools;

pub use server::{run, run_http, JournalMcpServer, RunConfig};
