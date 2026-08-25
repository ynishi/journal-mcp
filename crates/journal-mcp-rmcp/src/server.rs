//! [`JournalMcpServer`]: the `ServerHandler` implementation, [`RunConfig`],
//! and the [`run`] entry point.
//!
//! MCP Protocol (stdio) <-> journal_mcp_core::JournalCore
//!
//! Environment-variable resolution (`JOURNAL_PROJECT_ROOT` /
//! `JOURNAL_FILE_ENABLE` / `JOURNAL_FILE_OUTPUT_PATH`) is **not** performed
//! here — it is the caller's responsibility (typically the `journal-mcp`
//! binary crate) to resolve those into a [`RunConfig`] before calling
//! [`run`].  This keeps the rmcp interface layer environment-agnostic and
//! embeddable by other MCP hosts.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rmcp::{
    handler::server::router::tool::ToolRouter,
    model::{ProtocolVersion, ServerCapabilities, ServerInfo},
    tool_handler,
    transport::stdio,
    ServerHandler, ServiceExt,
};

// ---------------------------------------------------------------------------
// RunConfig / run — public entry point
// ---------------------------------------------------------------------------

/// Configuration for constructing and running a [`JournalMcpServer`].
///
/// Environment-variable resolution is the caller's responsibility; this
/// crate never reads `JOURNAL_*` env vars to decide server behaviour — it
/// only acts on the already-resolved values carried by this struct.
pub struct RunConfig {
    /// Root directory of the project this server instance manages.
    pub project_root: PathBuf,
    /// `Some(path)` attaches a `FileProjection` at `path` (the caller must
    /// have already resolved relative paths against `project_root`).
    /// `None` means EventLog-only startup — no projection is attached and
    /// chapter content is read back through MCP tools (`journal_tail` /
    /// `journal_grep` / `journal_chapter_list` / `journal_progress_of`).
    pub file_projection: Option<PathBuf>,
}

/// Construct a [`JournalMcpServer`] from `cfg` and serve it over stdio.
///
/// # Crux #3 (revised)
///
/// stdio is the **default** transport and this function wires exactly
/// `server.serve(stdio()).await?.waiting().await?`.  The original v0.1.0
/// invariant ("must not be replaced with another transport") is revised for
/// remote mode: the streamable-HTTP daemon transport lives in the separate
/// [`run_http`] entry point.  This function itself remains stdio-only.
///
/// # Errors
///
/// Returns an error if [`JournalMcpServer::new`] fails, or if the stdio
/// transport fails to start.
pub async fn run(cfg: RunConfig) -> anyhow::Result<()> {
    tracing::info!(project_root = ?cfg.project_root, "journal-mcp starting");
    let server = JournalMcpServer::new(cfg)?;
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// Serve over streamable HTTP transport (multi-device / SSOT-daemon mode).
///
/// One central daemon owns the EventLog SQLite databases
/// (`{project_root}/workspace/.journal.db` per project, via the per-call
/// `project_root` override + `extra_cores` cache); remote devices connect as
/// MCP clients to `http://<bind>/mcp`.  Because every device talks to the
/// same single process, the existing single-writer storage model is
/// preserved and no cross-device sync/conflict handling is needed.  Clients
/// that want a local `journal.md` call the `journal_dump` tool and write the
/// returned Markdown themselves — the daemon never writes files on the
/// client's behalf.
///
/// # Security model
///
/// - Loopback bind (e.g. `127.0.0.1:8487`): token optional.  rmcp's default
///   `Host` header validation (loopback-only) guards against DNS rebinding.
/// - Non-loopback bind: `JOURNAL_MCP_HTTP_TOKEN` **must** be set or startup
///   is refused.  When a token is set, every request must carry
///   `Authorization: Bearer <token>`.  Host validation is disabled in this
///   case (the bearer token replaces it); TLS termination, if desired, is a
///   reverse-proxy concern.
///
/// # Errors
///
/// Returns an error if [`JournalMcpServer::new`] fails, the bind address is
/// invalid, a non-loopback bind is requested without a token, or the HTTP
/// server fails to start.
pub async fn run_http(cfg: RunConfig, bind: &str) -> anyhow::Result<()> {
    use std::sync::Arc;

    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    };

    tracing::info!(project_root = ?cfg.project_root, "journal-mcp starting (http)");
    let server = JournalMcpServer::new(cfg)?;

    let addr: std::net::SocketAddr = bind
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid --bind address {bind:?}: {e}"))?;
    let token = std::env::var("JOURNAL_MCP_HTTP_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
        .map(Arc::<str>::from);
    let loopback = addr.ip().is_loopback();
    if !loopback && token.is_none() {
        anyhow::bail!(
            "refusing to bind non-loopback address {addr} without JOURNAL_MCP_HTTP_TOKEN \
             (set the token, or bind a loopback address)"
        );
    }

    let mut http_config = StreamableHttpServerConfig::default();
    if !loopback {
        // Bearer auth replaces loopback Host validation for LAN/remote binds.
        http_config.allowed_hosts.clear();
    }
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        http_config,
    );

    let mut router = axum::Router::new().nest_service("/mcp", service);
    if let Some(token) = token {
        router = router.layer(axum::middleware::from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let token = Arc::clone(&token);
                async move {
                    if bearer_token_matches(req.headers(), &token) {
                        next.run(req).await
                    } else {
                        axum::response::IntoResponse::into_response((
                            axum::http::StatusCode::UNAUTHORIZED,
                            "missing or invalid bearer token",
                        ))
                    }
                }
            },
        ));
    }

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, auth = if loopback { "loopback" } else { "bearer-token" }, "serving MCP over streamable HTTP at /mcp");
    axum::serve(listener, router).await?;
    Ok(())
}

/// Constant-time comparison of the request's `Authorization: Bearer` value
/// against the configured token.
fn bearer_token_matches(headers: &axum::http::HeaderMap, token: &str) -> bool {
    let Some(presented) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    else {
        return false;
    };
    let (a, b) = (presented.as_bytes(), token.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ---------------------------------------------------------------------------
// JournalMcpServer
// ---------------------------------------------------------------------------

/// MCP server for the `journal-mcp-core` library.
///
/// Wraps a [`journal_mcp_core::JournalCore`] behind an `Arc<Mutex<…>>` so that the
/// server can be cloned per-connection as required by rmcp while the core
/// remains single-writer.
///
/// # Crux invariants satisfied here
///
/// * **Crux #1** (tool_router 一元 ServerHandler 配線): all 17 tools are
///   registered in a single `#[tool_router] impl JournalMcpServer` block
///   (in `tools.rs`) and dispatched through `#[tool_handler] impl ServerHandler`
///   (below).
/// * **Crux #3** (stdio default 配線, revised): [`run`] wires
///   `server.serve(stdio()).await?.waiting().await?` and stays stdio-only;
///   the streamable-HTTP daemon transport is the separate [`run_http`]
///   entry point (remote mode).
#[derive(Clone)]
pub struct JournalMcpServer {
    /// ToolRouter is stored in the struct so that `list_all()` is available
    /// in integration tests without needing a live MCP session.
    #[allow(dead_code)]
    pub(crate) tool_router: ToolRouter<Self>,
    /// Shared mutable journal core — single writer, `std::sync::Mutex` is
    /// sufficient because we never `.await` while holding the lock guard.
    /// Default core, rooted at the startup-time `project_root`.
    pub(crate) core: Arc<Mutex<journal_mcp_core::JournalCore>>,
    /// Startup-time project root (used when a tool call omits `project_root`).
    pub(crate) project_root: PathBuf,
    /// Per-project lazy cache.  Keyed by canonicalized project_root path,
    /// value is an `Arc<Mutex<JournalCore>>` that owns the `.journal.db` /
    /// `journal.md` for that project.  Populated on the first tool call
    /// that supplies a non-default `project_root` argument.
    pub(crate) extra_cores: Arc<Mutex<HashMap<PathBuf, Arc<Mutex<journal_mcp_core::JournalCore>>>>>,
    /// Absolute path to the `.journal.db` file, captured at startup.
    /// Used by `journal_info` to avoid repeating the path-construction literal.
    pub(crate) db_path: PathBuf,
    /// Absolute path to the project-local schema directory
    /// (`<project_root>/.journal/schemas`), captured at startup.
    pub(crate) schema_registry_path: PathBuf,
    /// Server startup time formatted as RFC3339 (UTC), captured once in `build()`.
    pub(crate) started_at: String,
    /// Value of `JOURNAL_PROJECT_ROOT` env var at startup, if set.
    ///
    /// This is a diagnostic passthrough (surfaced by `journal_info`), not a
    /// behavioural decision, so it is read directly here rather than routed
    /// through [`RunConfig`].
    pub(crate) env_journal_project_root: Option<PathBuf>,
    /// Absolute path to the startup-attached FileProjection output, or
    /// `None` when no `FileProjection` was attached at startup (the v0.4.0
    /// default unless `JOURNAL_FILE_ENABLE` is set).
    ///
    /// Used by `journal_info` to expose the resolved path to MCP callers,
    /// and by `resolve_core` to decide whether per-call `project_root`
    /// overrides also get a default-location `FileProjection`.
    /// `journal_projection_rebuild` ignores this field; per-call output paths
    /// always resolve against `project_root` (or are absolute).
    pub(crate) file_projection_path: Option<PathBuf>,
}

impl JournalMcpServer {
    /// Construct a `JournalMcpServer` from an already-resolved [`RunConfig`].
    ///
    /// Performs the following initialisation steps:
    ///
    /// 1. Load the schema registry (`SchemaRegistry::with_project_local`).
    /// 2. Open (or create) the journal database at
    ///    `{project_root}/workspace/.journal.db`.
    /// 3. Optionally attach a [`FileProjection`](journal_mcp_core::FileProjection)
    ///    at `cfg.file_projection`, when `Some`.
    ///
    /// For tests that need deterministic FileProjection control without
    /// building a full [`RunConfig`], use
    /// [`new_with_file_attach`](Self::new_with_file_attach) /
    /// [`new_without_file_attach`](Self::new_without_file_attach)
    /// (available under `#[cfg(test)]`).
    ///
    /// # Errors
    ///
    /// Returns an error if the schema registry or database cannot be opened.
    pub fn new(cfg: RunConfig) -> anyhow::Result<Self> {
        Self::build(cfg.project_root, cfg.file_projection)
    }

    /// Internal constructor shared by `new` and the test-only ctors.
    ///
    /// `file_projection` is `Some(path)` (already resolved to an absolute
    /// path by the caller) to attach a `FileProjection`, or `None` to run
    /// EventLog-only.
    ///
    /// # Errors
    ///
    /// Returns an error if the schema registry or database cannot be opened.
    fn build(project_root: PathBuf, file_projection: Option<PathBuf>) -> anyhow::Result<Self> {
        let db_dir = project_root.join("workspace");
        // Scan for stale .bak.* files before opening the database.
        let stale = Self::detect_stale_bak_files(&db_dir);
        for p in &stale {
            tracing::warn!(
                target: "journal::startup",
                path = ?p,
                "stale .bak file detected, ensure stop-before-mv was followed in last migration (ignore if this backup is intentional)"
            );
        }

        let (core, db_path, file_projection_path) =
            Self::build_core(&project_root, file_projection)?;
        // After build_core succeeds, the project_root (and its workspace
        // subdir) exists, so `canonicalize` resolves all symlinks (e.g. on
        // macOS where TempDir lives under `/var` → `/private/var`).  This
        // canonical form is what `resolve_core` compares against, so storing
        // it here makes the per-call override short-circuit work reliably.
        let canonical_root = std::fs::canonicalize(&project_root).unwrap_or(project_root);

        let schema_registry_path = canonical_root.join(".journal").join("schemas");

        let started_at = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|e| {
                tracing::warn!(target: "journal::startup", error = ?e, "failed to format startup time as RFC3339");
                String::from("unknown")
            });

        let env_journal_project_root = std::env::var_os("JOURNAL_PROJECT_ROOT").map(PathBuf::from);

        Ok(Self {
            tool_router: Self::tool_router(),
            core: Arc::new(Mutex::new(core)),
            project_root: canonical_root,
            extra_cores: Arc::new(Mutex::new(HashMap::new())),
            db_path,
            schema_registry_path,
            started_at,
            env_journal_project_root,
            file_projection_path,
        })
    }

    /// Build a fresh `JournalCore` rooted at the given `project_root`.
    ///
    /// Shared by the startup-time constructor (`build`) and the per-call
    /// lazy-cache populator (`resolve_core`).  Each call opens an independent
    /// SQLite handle at `{project_root}/workspace/.journal.db` and attaches a
    /// `FileProjection` at `file_projection` when `Some` (the path must
    /// already be resolved — absolute, or relative to the given
    /// `project_root` by the caller).
    ///
    /// Returns `(JournalCore, db_path, file_projection_path)` where
    /// `file_projection_path` echoes back the `file_projection` argument.
    ///
    /// # Errors
    ///
    /// Returns an error if the schema registry or database cannot be opened.
    fn build_core(
        project_root: &Path,
        file_projection: Option<PathBuf>,
    ) -> anyhow::Result<(journal_mcp_core::JournalCore, PathBuf, Option<PathBuf>)> {
        let registry = journal_mcp_core::SchemaRegistry::with_project_local(project_root)?;

        let db_dir = project_root.join("workspace");
        // Ensure the workspace directory exists so SQLite can create the DB file.
        std::fs::create_dir_all(&db_dir)?;
        let db_path = db_dir.join(".journal.db");

        let resolved_path = file_projection;

        // Clone the registry before consuming it; FileProjection needs an Arc.
        let registry_arc = std::sync::Arc::new(registry.clone());
        let mut core = journal_mcp_core::JournalCore::open(&db_path, registry)?;

        if let Some(ref path) = resolved_path {
            let proj = journal_mcp_core::FileProjection::new(path.clone(), registry_arc);
            core.add_projection(proj);
        }

        Ok((core, db_path, resolved_path))
    }

    /// Apply pagination (offset + limit) to a `Vec<T>`.
    ///
    /// Semantics:
    /// - `offset = None` (or `Some(0)`) → no items are skipped.
    /// - `limit = None` → return all items from `offset` onwards.
    /// - `offset >= len` → empty `Vec<T>` (not an error).
    ///
    /// Used by `journal_chapter_list` to page large chapter sets without
    /// exceeding MCP client output size limits.  Decoupled from the tool
    /// handler so the slicing semantics are unit-testable without spinning
    /// up a full `JournalCore` + `tokio` runtime.
    pub(crate) fn paginate<T>(
        items: Vec<T>,
        offset: Option<usize>,
        limit: Option<usize>,
    ) -> Vec<T> {
        items
            .into_iter()
            .skip(offset.unwrap_or(0))
            .take(limit.unwrap_or(usize::MAX))
            .collect()
    }

    /// Resolve the `JournalCore` handle for the given optional per-call `project_root`.
    ///
    /// * `None` — return a clone of the startup-time default `core` handle.
    /// * `Some(path)` — canonicalize the path; if it matches the default
    ///   `project_root`, return the default handle (no extra DB open).  Otherwise
    ///   look up the path in the per-project lazy cache (`extra_cores`); on cache
    ///   miss, build a fresh `JournalCore` rooted at that path, insert it into
    ///   the cache, and return its handle.  The cached core attaches a default
    ///   `<project_root>/workspace/journal.md` `FileProjection` when the
    ///   startup (default) core has one attached (i.e. `self.file_projection_path`
    ///   is `Some`), mirroring the startup enable/disable policy across
    ///   per-call project overrides; a custom absolute/relative override used
    ///   for the default project's `FileProjection` is not carried over to
    ///   extra projects (each gets its own default location).
    ///
    /// Concurrency: holds the `extra_cores` HashMap lock only across the
    /// insert/lookup; the lock guard is dropped before returning the cloned
    /// `Arc<Mutex<JournalCore>>` handle to the caller, so per-`JournalCore`
    /// operations do not serialise across projects.
    ///
    /// # Errors
    ///
    /// Returns an error if `build_core` fails for the requested path.
    pub(crate) fn resolve_core(
        &self,
        project_root: Option<&str>,
    ) -> anyhow::Result<Arc<Mutex<journal_mcp_core::JournalCore>>> {
        let Some(pr) = project_root else {
            return Ok(self.core.clone());
        };
        let pr_path = PathBuf::from(pr);
        let canonical = std::fs::canonicalize(&pr_path).unwrap_or(pr_path);
        // Short-circuit: if it canonicalizes to the default project_root, reuse the default core.
        if canonical == self.project_root {
            return Ok(self.core.clone());
        }
        let mut extra = self
            .extra_cores
            .lock()
            .map_err(|e| anyhow::anyhow!("extra_cores mutex poisoned: {e}"))?;
        if let Some(c) = extra.get(&canonical) {
            return Ok(c.clone());
        }
        let extra_file_projection = self
            .file_projection_path
            .as_ref()
            .map(|_| canonical.join("workspace").join("journal.md"));
        let (core, _db_path, _file_projection_path) =
            Self::build_core(&canonical, extra_file_projection)?;
        let handle = Arc::new(Mutex::new(core));
        extra.insert(canonical, handle.clone());
        Ok(handle)
    }

    /// Test-only constructor that force-attaches a `FileProjection` at the
    /// given path, ignoring env vars.
    ///
    /// Relative paths resolve against `project_root`; absolute paths are
    /// used as-is.  Use this when a test needs deterministic FileProjection
    /// I/O without touching `JOURNAL_FILE_ENABLE` / `JOURNAL_FILE_OUTPUT_PATH`
    /// (which would race with other parallel tests).
    ///
    /// # Errors
    ///
    /// Returns an error if the schema registry or database cannot be opened.
    #[cfg(test)]
    pub(crate) fn new_with_file_attach(
        project_root: PathBuf,
        output_path: PathBuf,
    ) -> anyhow::Result<Self> {
        let resolved = if output_path.is_absolute() {
            output_path
        } else {
            project_root.join(&output_path)
        };
        Self::build(project_root, Some(resolved))
    }

    /// Test-only constructor that force-disables the startup `FileProjection`,
    /// ignoring env vars.
    ///
    /// Use this when a test needs to verify behaviour without any file I/O
    /// (e.g. tool-registration tests, schema tests).  All file paths remain
    /// inside the `TempDir` regardless of env state (Crux #3 test-isolation
    /// requirement).
    ///
    /// # Errors
    ///
    /// Returns an error if the schema registry or database cannot be opened.
    #[cfg(test)]
    pub(crate) fn new_without_file_attach(project_root: PathBuf) -> anyhow::Result<Self> {
        Self::build(project_root, None)
    }

    /// Scan `db_dir` for stale backup files left by a previous migration.
    ///
    /// Returns every entry whose filename starts with one of the three known
    /// backup prefixes: `.journal.db.bak.`, `.journal.db-wal.bak.`, or
    /// `.journal.db-shm.bak.`.  Read errors are logged as warnings and an
    /// empty `Vec` is returned so startup always continues.
    pub(crate) fn detect_stale_bak_files(db_dir: &std::path::Path) -> Vec<PathBuf> {
        const PREFIXES: &[&str] = &[
            ".journal.db.bak.",
            ".journal.db-wal.bak.",
            ".journal.db-shm.bak.",
        ];

        let entries = match std::fs::read_dir(db_dir) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(
                    target: "journal::startup",
                    error = ?e,
                    dir = ?db_dir,
                    "failed to scan workspace dir for stale .bak files"
                );
                return Vec::new();
            }
        };

        let mut found = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(
                        target: "journal::startup",
                        error = ?e,
                        dir = ?db_dir,
                        "failed to scan workspace dir for stale .bak files"
                    );
                    continue;
                }
            };
            let file_name = entry.file_name();
            let name = match file_name.to_str() {
                Some(s) => s,
                None => continue,
            };
            if PREFIXES.iter().any(|prefix| name.starts_with(prefix)) {
                found.push(entry.path());
            }
        }
        found
    }
}

// ---------------------------------------------------------------------------
// ServerHandler — Crux #1: #[tool_handler] macro wires tool_router dispatch
// ---------------------------------------------------------------------------

/// `ServerHandler` implementation for `JournalMcpServer`.
///
/// The `#[tool_handler]` macro generates the `call_tool` dispatch that routes
/// MCP wire calls to the correct `#[tool_router]` method (registered in
/// `tools.rs`).  Only `get_info` is manually implemented here; all other
/// `ServerHandler` methods keep their default no-op implementations.
#[tool_handler]
impl ServerHandler for JournalMcpServer {
    fn get_info(&self) -> ServerInfo {
        let caps = ServerCapabilities::builder().enable_tools().build();
        let impl_info = rmcp::model::Implementation::new("journal-mcp", env!("CARGO_PKG_VERSION"))
            .with_title("Journal MCP — project canonical history")
            .with_description(
                "MCP server exposing the journal library for project canonical history management.",
            );
        ServerInfo::new(caps)
            .with_server_info(impl_info)
            .with_protocol_version(ProtocolVersion::V_2025_03_26)
            .with_instructions(
                "journal-mcp: open chapters, append sections, manage schemas and projections.",
            )
    }
}
