//! Thin entry point for the journal-mcp MCP server.
//!
//! Resolves `JOURNAL_PROJECT_ROOT` / `JOURNAL_FILE_ENABLE` /
//! `JOURNAL_FILE_OUTPUT_PATH` env vars into a `RunConfig` and delegates to
//! [`journal_mcp_rmcp::run`] (stdio, default) or
//! [`journal_mcp_rmcp::run_http`] (`--mcp-http`, streamable HTTP daemon).
//! See `docs/design.md §6` for the tool table.
//!
//! Usage:
//!
//! ```text
//! journal-mcp                          # stdio (default)
//! journal-mcp --mcp-http [--bind ADDR] # streamable HTTP daemon
//! ```
//!
//! `--bind` defaults to `127.0.0.1:8487`. Non-loopback binds require
//! `JOURNAL_MCP_HTTP_TOKEN` (see `journal_mcp_rmcp::run_http`).

mod env_resolve;

use std::path::PathBuf;

use journal_mcp_rmcp::{run, run_http, RunConfig};

/// Default bind address for `--mcp-http` (loopback; SSOT-daemon deployments
/// override with `--bind 0.0.0.0:8487` + `JOURNAL_MCP_HTTP_TOKEN`).
const DEFAULT_BIND: &str = "127.0.0.1:8487";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let mut http = false;
    let mut bind: Option<String> = None;
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--mcp-http" => http = true,
            "--bind" => {
                let value = iter
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--bind requires a value (host:port)"))?;
                bind = Some(value);
            }
            other => {
                anyhow::bail!("unrecognized argument: {other} (expected --mcp-http / --bind ADDR)");
            }
        }
    }
    if bind.is_some() && !http {
        anyhow::bail!("--bind only applies with --mcp-http");
    }

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

    let cfg = RunConfig {
        project_root,
        file_projection,
    };

    if http {
        let bind = bind.as_deref().unwrap_or(DEFAULT_BIND);
        run_http(cfg, bind).await
    } else {
        run(cfg).await
    }
}
