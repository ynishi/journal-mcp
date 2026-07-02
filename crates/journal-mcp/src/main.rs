//! Thin entry point for the journal-mcp stdio MCP server.
//!
//! Resolves `JOURNAL_PROJECT_ROOT` / `JOURNAL_FILE_ENABLE` /
//! `JOURNAL_FILE_OUTPUT_PATH` env vars into a `RunConfig` and delegates to
//! [`journal_mcp_rmcp::run`]. See `docs/design.md §6` for the tool table.

mod env_resolve;

use std::path::PathBuf;

use journal_mcp_rmcp::{run, RunConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let project_root = std::env::var("JOURNAL_PROJECT_ROOT")
        .map(PathBuf::from)
        // SAFETY: current_dir() can only fail if the process has no CWD.
        .unwrap_or_else(|_| std::env::current_dir().expect("cwd accessible at startup"));

    let enable_set = std::env::var_os("JOURNAL_FILE_ENABLE").is_some();
    let path_env = std::env::var_os("JOURNAL_FILE_OUTPUT_PATH");
    let (file_projection, warn_msg) =
        env_resolve::resolve_file_projection_path(&project_root, enable_set, path_env.as_deref());
    if let Some(msg) = warn_msg {
        tracing::warn!("{msg}");
    }

    run(RunConfig {
        project_root,
        file_projection,
    })
    .await
}
