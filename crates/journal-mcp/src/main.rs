//! journal-mcp — stdio MCP server exposing the `journal-mcp-core` library.
//!
//! Exposes the `journal-mcp-core` crate as an MCP server over stdio transport.
//! All tools are registered in a single [`#[tool_router]`](rmcp::tool_router) block
//! on [`JournalMcpServer`], satisfying the Crux #1 (tool_router 一元 ServerHandler 配線)
//! constraint.
//!
//! # Environment variables
//!
//! * `JOURNAL_PROJECT_ROOT` — root directory of the project.  Defaults to the
//!   process's current working directory.
//!
//! See `docs/design.md §6` for the full tool table and `§10 Step 5` for the
//! stdio transport specification.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ProtocolVersion, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
    transport::stdio,
    ServerHandler, ServiceExt,
};
use schemars::JsonSchema;
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Parameter structs — one per tool; doc comments become MCP wire descriptions
// ---------------------------------------------------------------------------

/// Parameters for `journal_open_chapter`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct JournalOpenChapterParams {
    /// Chapter name, typically a date slug such as `"2026-06-14"`.
    pub name: String,
    /// Schema ID that governs this chapter (e.g. `"journal-mcp-canonical-v1"`).
    pub schema_id: String,
    /// Optional per-call project_root override.  When `None`, the
    /// startup-time default is used.  When `Some(path)`, the server
    /// lazily opens (or reuses a cached) `JournalCore` rooted at the
    /// given path and executes this call against it.
    #[serde(default)]
    pub project_root: Option<String>,
}

/// Parameters for `journal_append_section`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct JournalAppendSectionParams {
    /// Target chapter ID (the value returned by `journal_open_chapter`).
    pub chapter_id: String,
    /// Name of the section to append (e.g. `"Verified"`).
    pub section_name: String,
    /// Body text of the section row.
    pub body: String,
    /// Optional per-call project_root override.  When `None`, the
    /// startup-time default is used.  When `Some(path)`, the server
    /// lazily opens (or reuses a cached) `JournalCore` rooted at the
    /// given path and executes this call against it.
    #[serde(default)]
    pub project_root: Option<String>,
}

/// Parameters for `journal_append_progress`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct JournalAppendProgressParams {
    /// Target chapter ID.
    pub chapter_id: String,
    /// Single progress line to append (e.g. `"step 3 done"`).
    pub line: String,
    /// Optional per-call project_root override.  When `None`, the
    /// startup-time default is used.  When `Some(path)`, the server
    /// lazily opens (or reuses a cached) `JournalCore` rooted at the
    /// given path and executes this call against it.
    #[serde(default)]
    pub project_root: Option<String>,
}

/// Parameters for `journal_close_chapter`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct JournalCloseChapterParams {
    /// Target chapter ID to close.
    pub chapter_id: String,
    /// Optional per-call project_root override.  When `None`, the
    /// startup-time default is used.  When `Some(path)`, the server
    /// lazily opens (or reuses a cached) `JournalCore` rooted at the
    /// given path and executes this call against it.
    #[serde(default)]
    pub project_root: Option<String>,
}

// ---------------------------------------------------------------------------
// Subtask-2 parameter structs — schema 3 tool + read 3 tool
// ---------------------------------------------------------------------------

/// Parameters for `journal_schema_load`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct JournalSchemaLoadParams {
    /// YAML literal conforming to the ChapterSchema format (see `docs/design.md §5`).
    pub yaml: String,
    /// Optional per-call project_root override.  When `None`, the
    /// startup-time default is used.  When `Some(path)`, the server
    /// lazily opens (or reuses a cached) `JournalCore` rooted at the
    /// given path and executes this call against it.
    #[serde(default)]
    pub project_root: Option<String>,
}

/// Parameters for `journal_schema_list` (no fields — lists all schemas).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct JournalSchemaListParams {
    /// Optional per-call project_root override.  When `None`, the
    /// startup-time `JOURNAL_PROJECT_ROOT` (or `current_dir()`) is used.
    /// When `Some(path)`, the server lazily opens (or reuses a cached)
    /// `JournalCore` rooted at the given path and executes this call
    /// against it.  Backward-compatible: omitting the field falls back
    /// to the default behaviour.
    #[serde(default)]
    pub project_root: Option<String>,
}

/// Parameters for `journal_schema_show`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct JournalSchemaShowParams {
    /// Registry key to look up (e.g. `"journal-mcp-canonical-v1"`).
    pub key: String,
    /// Optional per-call project_root override.  When `None`, the
    /// startup-time default is used.  When `Some(path)`, the server
    /// lazily opens (or reuses a cached) `JournalCore` rooted at the
    /// given path and executes this call against it.
    #[serde(default)]
    pub project_root: Option<String>,
}

/// Parameters for `journal_tail`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct JournalTailParams {
    /// Maximum number of chapters to return (default 10).
    pub n: Option<usize>,
    /// Optional per-call project_root override.  When `None`, the
    /// startup-time default is used.  When `Some(path)`, the server
    /// lazily opens (or reuses a cached) `JournalCore` rooted at the
    /// given path and executes this call against it.
    #[serde(default)]
    pub project_root: Option<String>,
}

/// Parameters for `journal_grep`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct JournalGrepParams {
    /// Substring pattern to search for in all section bodies.
    pub pattern: String,
    /// Optional start filter: only chapters opened at or after this Unix epoch ms.
    pub since: Option<i64>,
    /// Optional end filter: only chapters opened at or before this Unix epoch ms.
    pub until: Option<i64>,
    /// Optional per-call project_root override.  When `None`, the
    /// startup-time default is used.  When `Some(path)`, the server
    /// lazily opens (or reuses a cached) `JournalCore` rooted at the
    /// given path and executes this call against it.
    #[serde(default)]
    pub project_root: Option<String>,
}

/// Parameters for `journal_chapter_list` (supports pagination).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct JournalChapterListParams {
    /// Optional per-call project_root override.  When `None`, the
    /// startup-time `JOURNAL_PROJECT_ROOT` (or `current_dir()`) is used.
    /// When `Some(path)`, the server lazily opens (or reuses a cached)
    /// `JournalCore` rooted at the given path and executes this call
    /// against it.  Backward-compatible: omitting the field falls back
    /// to the default behaviour.
    #[serde(default)]
    pub project_root: Option<String>,
    /// Maximum number of chapters to return, applied after `offset`.
    /// When `None` (default), all remaining chapters are returned —
    /// preserves the pre-pagination behaviour.  Newest chapters first
    /// (i.e. position 0 is the most recently opened chapter).
    #[serde(default)]
    pub limit: Option<usize>,
    /// Number of chapters to skip from the start of the list, applied
    /// before `limit`.  When `None` (default), no chapters are skipped.
    /// Newest chapters first, so `offset=0` starts at the most recently
    /// opened chapter.  An `offset` greater than or equal to the total
    /// chapter count yields an empty result (not an error).
    #[serde(default)]
    pub offset: Option<usize>,
}

// ---------------------------------------------------------------------------
// Subtask-3 parameter structs — open_chapters / progress_of / projection 3
// ---------------------------------------------------------------------------

/// Parameters for `journal_open_chapters` (no fields — lists all open chapters).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct JournalOpenChaptersParams {
    /// Optional per-call project_root override.  When `None`, the
    /// startup-time `JOURNAL_PROJECT_ROOT` (or `current_dir()`) is used.
    /// When `Some(path)`, the server lazily opens (or reuses a cached)
    /// `JournalCore` rooted at the given path and executes this call
    /// against it.  Backward-compatible: omitting the field falls back
    /// to the default behaviour.
    #[serde(default)]
    pub project_root: Option<String>,
}

/// Parameters for `journal_progress_of`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct JournalProgressOfParams {
    /// Target chapter ID whose Progress section events to return.
    pub chapter_id: String,
    /// Optional per-call project_root override.  When `None`, the
    /// startup-time default is used.  When `Some(path)`, the server
    /// lazily opens (or reuses a cached) `JournalCore` rooted at the
    /// given path and executes this call against it.
    #[serde(default)]
    pub project_root: Option<String>,
}

/// Parameters for `journal_projection_attach`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct JournalProjectionAttachParams {
    /// Stable name of the projection to attach (e.g. `"file"`).
    pub name: String,
    /// Optional per-call project_root override.  When `None`, the
    /// startup-time default is used.  When `Some(path)`, the server
    /// lazily opens (or reuses a cached) `JournalCore` rooted at the
    /// given path and executes this call against it.
    #[serde(default)]
    pub project_root: Option<String>,
}

/// Parameters for `journal_projection_detach`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct JournalProjectionDetachParams {
    /// Stable name of the projection to detach (e.g. `"file"`).
    pub name: String,
    /// Optional per-call project_root override.  When `None`, the
    /// startup-time default is used.  When `Some(path)`, the server
    /// lazily opens (or reuses a cached) `JournalCore` rooted at the
    /// given path and executes this call against it.
    #[serde(default)]
    pub project_root: Option<String>,
}

/// Parameters for `journal_projection_rebuild`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct JournalProjectionRebuildParams {
    /// Stable name of the projection to rebuild (e.g. `"file"`).
    pub name: String,
    /// Optional per-call project_root override.  When `None`, the
    /// startup-time default is used.  When `Some(path)`, the server
    /// lazily opens (or reuses a cached) `JournalCore` rooted at the
    /// given path and executes this call against it.
    #[serde(default)]
    pub project_root: Option<String>,
    /// Optional per-call output path override for a one-shot rebuild.
    /// When `Some(path)`, the projection is rebuilt to this path instead of
    /// the default attached path.  Relative paths are resolved against
    /// `project_root` (or the startup-time default when `project_root` is
    /// `None`).  Absolute paths are used as-is.  The default attached
    /// projection is **not** modified — subsequent `close_chapter` writes
    /// still go to the default attached path.  Only meaningful when
    /// `name == "file"`; for other projection names the argument is ignored
    /// and a warning is logged.
    #[serde(default)]
    pub output_path: Option<String>,
}

// ---------------------------------------------------------------------------
// ST7 parameter structs — import tool
// ---------------------------------------------------------------------------

/// Parameters for `journal_import`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct JournalImportParams {
    /// Filesystem path of the markdown file to import (absolute or relative to
    /// `JOURNAL_PROJECT_ROOT`).
    pub path: String,
    /// Optional per-call project_root override.  When `None`, the
    /// startup-time default is used.  When `Some(path)`, the server
    /// lazily opens (or reuses a cached) `JournalCore` rooted at the
    /// given path and executes this call against it.
    #[serde(default)]
    pub project_root: Option<String>,
}

// ---------------------------------------------------------------------------
// Local output structs (not in the journal crate, MCP-layer only)
// ---------------------------------------------------------------------------

/// A row in the `journal_chapter_list` response table (Microsoft Decision Log format).
#[derive(Debug, serde::Serialize)]
struct ChapterListRow {
    /// Chapter date or identifier slug.
    chapter_id: String,
    /// Schema used for this chapter.
    schema_id: String,
    /// Current state machine state (e.g. `"closed"`, `"open"`).
    current_state: String,
    /// Unix epoch milliseconds when the chapter was opened.
    opened_at: i64,
    /// Unix epoch milliseconds when the chapter was closed, or `null`.
    closed_at: Option<i64>,
    /// First line of the `Decided` section (empty string if absent).
    decided_summary: String,
    /// Anchor link for the `Decided` section.
    link: String,
}

// ---------------------------------------------------------------------------
// JournalInfoResult — return type for the `journal_info` diagnostic tool
// ---------------------------------------------------------------------------

/// Snapshot of the server's runtime state, returned by `journal_info`.
///
/// All path fields are absolute and resolved at server startup time.
/// Consumers can use this to diagnose path resolution, confirm which
/// database the server is using, and enumerate available schemas.
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct JournalInfoResult {
    /// Project root path resolved at server startup (canonicalized).
    pub project_root: PathBuf,
    /// Absolute path to the `.journal.db` file that the server is using.
    /// Always `<project_root>/workspace/.journal.db` (literal-fixed, no fallback).
    pub db_path: PathBuf,
    /// `true` if `db_path` exists on the filesystem at the time `journal_info` is called.
    pub db_exists: bool,
    /// Absolute path to the WAL companion file (`-wal` suffix).
    pub wal_path: PathBuf,
    /// Absolute path to the shared-memory companion file (`-shm` suffix).
    pub shm_path: PathBuf,
    /// Project-local schema directory (`<project_root>/.journal/schemas`).
    pub schema_registry_path: PathBuf,
    /// All registered schema keys (L1 built-in ∪ L2 project-local, L2 wins, de-duplicated).
    pub available_schemas: Vec<String>,
    /// Crate version (e.g. `"0.1.0"`).
    pub version: String,
    /// Server startup time in RFC3339 (UTC).
    pub startup_time: String,
    /// `JOURNAL_PROJECT_ROOT` env var value at startup (if set).
    pub env_journal_project_root: Option<PathBuf>,
    /// Absolute path to the FileProjection output (current attached default).
    /// Default: `<project_root>/journal.md` (root).
    pub file_projection_path: PathBuf,
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
/// * **Crux #1** (tool_router 一元 ServerHandler 配線): all 17 tools (ST6-1/ST6-2/ST6-3/ST7)
///   are registered in a single `#[tool_router] impl JournalMcpServer` block and
///   dispatched through `#[tool_handler] impl ServerHandler`.
/// * **Crux #3** (stdio transport 固定配線): `main()` wires
///   `server.serve(stdio()).await?.waiting().await?` and never uses another transport.
#[derive(Clone)]
pub struct JournalMcpServer {
    /// ToolRouter is stored in the struct so that `list_all()` is available
    /// in integration tests without needing a live MCP session.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
    /// Shared mutable journal core — single writer, `std::sync::Mutex` is
    /// sufficient because we never `.await` while holding the lock guard.
    /// Default core, rooted at the startup-time `project_root`.
    core: Arc<Mutex<journal_mcp_core::JournalCore>>,
    /// Startup-time project root (used when a tool call omits `project_root`).
    project_root: PathBuf,
    /// Per-project lazy cache.  Keyed by canonicalized project_root path,
    /// value is an `Arc<Mutex<JournalCore>>` that owns the `.journal.db` /
    /// `journal.md` for that project.  Populated on the first tool call
    /// that supplies a non-default `project_root` argument.
    extra_cores: Arc<Mutex<HashMap<PathBuf, Arc<Mutex<journal_mcp_core::JournalCore>>>>>,
    /// Absolute path to the `.journal.db` file, captured at startup.
    /// Used by `journal_info` to avoid repeating the path-construction literal.
    db_path: PathBuf,
    /// Absolute path to the project-local schema directory
    /// (`<project_root>/.journal/schemas`), captured at startup.
    schema_registry_path: PathBuf,
    /// Server startup time formatted as RFC3339 (UTC), captured once in `build()`.
    started_at: String,
    /// Value of `JOURNAL_PROJECT_ROOT` env var at startup, if set.
    env_journal_project_root: Option<PathBuf>,
    /// Absolute path to the default-attached FileProjection output,
    /// captured at startup.  Used by `journal_info` and as the fallback
    /// when `journal_projection_rebuild` is called without `output_path`.
    file_projection_path: PathBuf,
}

impl JournalMcpServer {
    /// Construct a `JournalMcpServer` for the given `project_root`.
    ///
    /// Performs the following initialisation steps:
    ///
    /// 1. Load the schema registry (`SchemaRegistry::with_project_local`).
    /// 2. Open (or create) the journal database at
    ///    `{project_root}/workspace/.journal.db`.
    /// 3. Attach a [`FileProjection`](journal_mcp_core::FileProjection) that writes to
    ///    `{project_root}/journal.md` (root).
    ///
    /// For tests that need to suppress FileProjection I/O, use
    /// [`new_with_config`](Self::new_with_config) (available under `#[cfg(test)]`).
    ///
    /// # Errors
    ///
    /// Returns an error if the schema registry or database cannot be opened.
    pub fn new(project_root: PathBuf) -> anyhow::Result<Self> {
        Self::build(project_root, true)
    }

    /// Internal constructor shared by `new` and the test-only `new_with_config`.
    ///
    /// When `attach_file_projection` is `true`, a [`FileProjection`](journal_mcp_core::FileProjection)
    /// is attached at `{project_root}/journal.md` (root).  When `false`, no projection
    /// is attached.
    ///
    /// # Errors
    ///
    /// Returns an error if the schema registry or database cannot be opened.
    fn build(project_root: PathBuf, attach_file_projection: bool) -> anyhow::Result<Self> {
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

        let (core, db_path, _file_projection_path) =
            Self::build_core(&project_root, attach_file_projection)?;
        // After build_core succeeds, the project_root (and its workspace
        // subdir) exists, so `canonicalize` resolves all symlinks (e.g. on
        // macOS where TempDir lives under `/var` → `/private/var`).  This
        // canonical form is what `resolve_core` compares against, so storing
        // it here makes the per-call override short-circuit work reliably.
        let canonical_root = std::fs::canonicalize(&project_root).unwrap_or(project_root);
        // Derive file_projection_path from the already-canonicalized root.
        // We cannot call canonicalize on journal.md itself because the file does
        // not exist yet at startup; using canonical_root.join avoids the
        // /var → /private/var symlink mismatch on macOS.
        let file_projection_path = canonical_root.join("journal.md");

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
    /// SQLite handle at `{project_root}/workspace/.journal.db` and (optionally)
    /// attaches a `FileProjection` rendering to
    /// `{project_root}/journal.md` (root).
    ///
    /// Returns `(JournalCore, db_path, file_projection_path)`.
    ///
    /// # Errors
    ///
    /// Returns an error if the schema registry or database cannot be opened.
    fn build_core(
        project_root: &Path,
        attach_file_projection: bool,
    ) -> anyhow::Result<(journal_mcp_core::JournalCore, PathBuf, PathBuf)> {
        let registry = journal_mcp_core::SchemaRegistry::with_project_local(project_root)?;

        let db_dir = project_root.join("workspace");
        // Ensure the workspace directory exists so SQLite can create the DB file.
        std::fs::create_dir_all(&db_dir)?;
        let db_path = db_dir.join(".journal.db");

        // Default FileProjection output path: <project_root>/journal.md (root).
        // db_dir is kept for .journal.db placement only.
        let journal_md = project_root.join("journal.md");

        // Clone the registry before consuming it; FileProjection needs an Arc.
        let registry_arc = std::sync::Arc::new(registry.clone());
        let mut core = journal_mcp_core::JournalCore::open(&db_path, registry)?;

        if attach_file_projection {
            let proj = journal_mcp_core::FileProjection::new(journal_md.clone(), registry_arc);
            core.add_projection(proj);
        }

        Ok((core, db_path, journal_md))
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
    fn paginate<T>(items: Vec<T>, offset: Option<usize>, limit: Option<usize>) -> Vec<T> {
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
    ///   the cache, and return its handle.  `FileProjection` is always attached
    ///   for cached cores.
    ///
    /// Concurrency: holds the `extra_cores` HashMap lock only across the
    /// insert/lookup; the lock guard is dropped before returning the cloned
    /// `Arc<Mutex<JournalCore>>` handle to the caller, so per-`JournalCore`
    /// operations do not serialise across projects.
    ///
    /// # Errors
    ///
    /// Returns an error if `build_core` fails for the requested path.
    fn resolve_core(
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
        let (core, _db_path, _file_projection_path) = Self::build_core(&canonical, true)?;
        let handle = Arc::new(Mutex::new(core));
        extra.insert(canonical, handle.clone());
        Ok(handle)
    }

    /// Test-only constructor with explicit FileProjection control.
    ///
    /// Delegates to [`build`](Self::build) with the given `attach_file_projection`
    /// flag.  Pass `false` to suppress projection I/O in unit tests so that all
    /// file paths remain inside a `TempDir` (Crux #3 test-isolation requirement).
    ///
    /// This method is intentionally hidden from production builds.  If production
    /// code needs FileProjection control in the future, promote `build` to `pub`
    /// at that point (YAGNI for now).
    ///
    /// # Errors
    ///
    /// Returns an error if the schema registry or database cannot be opened.
    #[cfg(test)]
    pub(crate) fn new_with_config(
        project_root: PathBuf,
        attach_file_projection: bool,
    ) -> anyhow::Result<Self> {
        Self::build(project_root, attach_file_projection)
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
// Tool implementations — Crux #1: all tools in a single #[tool_router] block
// ---------------------------------------------------------------------------

/// All journal-mcp MCP tools are registered in this single block.
///
/// Subtask 2 (schema 3 tool + read 3 tool) and Subtask 3 (remaining 5 tool)
/// will add entries to **this same block** — never to a separate `impl` block.
/// This satisfies Crux #1 (tool_router 一元 ServerHandler 配線).
#[tool_router]
impl JournalMcpServer {
    /// Open a new journal chapter and return its chapter ID.
    ///
    /// Creates a chapter entry governed by the specified schema.
    /// The returned chapter ID must be passed to subsequent append/close calls.
    #[tool(
        name = "journal_open_chapter",
        description = "Open a new journal chapter (name + schema_id) and return its chapter ID.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn journal_open_chapter(
        &self,
        Parameters(params): Parameters<JournalOpenChapterParams>,
    ) -> Result<String, String> {
        let id = {
            // SAFETY: Mutex::lock().unwrap() — poisoned Mutex means the process is
            // in an undefined state; abort is acceptable here.
            let core_handle = self
                .resolve_core(params.project_root.as_deref())
                .map_err(|e| format!("resolve_core: {e}"))?;
            let mut core = core_handle.lock().unwrap();
            core.open_chapter(&params.name, &params.schema_id)
                .map_err(|e| {
                    tracing::warn!(error = ?e, "journal_open_chapter failed");
                    e.to_string()
                })?
        }; // guard drops here — no await is held across the Mutex
        Ok(id.0)
    }

    /// Append a section row to an existing open chapter.
    ///
    /// Returns any hook warnings emitted by the schema as a JSON array.
    #[tool(
        name = "journal_append_section",
        description = "Append a section row to an open chapter. \
                       Returns JSON array of hook warnings (may be empty).",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn journal_append_section(
        &self,
        Parameters(params): Parameters<JournalAppendSectionParams>,
    ) -> Result<String, String> {
        let chapter_id = journal_mcp_core::ChapterId(params.chapter_id);
        let warnings = {
            // SAFETY: see journal_open_chapter
            let core_handle = self
                .resolve_core(params.project_root.as_deref())
                .map_err(|e| format!("resolve_core: {e}"))?;
            let mut core = core_handle.lock().unwrap();
            core.append_section(&chapter_id, &params.section_name, &params.body)
                .map_err(|e| {
                    tracing::warn!(error = ?e, "journal_append_section failed");
                    e.to_string()
                })?
        };
        // HookWarning does not derive Serialize; format each warning as a string.
        let warning_strs: Vec<String> = warnings
            .iter()
            .map(|w| format!("{}: {}: {}", w.kind, w.section, w.hint))
            .collect();
        Ok(warning_strs.join("\n"))
    }

    /// Append a single progress line to the `Progress` section of an open chapter.
    ///
    /// Equivalent to `journal_append_section` with `section_name = "Progress"`.
    /// Returns any hook warnings as a JSON array.
    #[tool(
        name = "journal_append_progress",
        description = "Append a single line to the 'Progress' section of an open chapter. \
                       Returns JSON array of hook warnings (may be empty).",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn journal_append_progress(
        &self,
        Parameters(params): Parameters<JournalAppendProgressParams>,
    ) -> Result<String, String> {
        let chapter_id = journal_mcp_core::ChapterId(params.chapter_id);
        let warnings = {
            // SAFETY: see journal_open_chapter
            let core_handle = self
                .resolve_core(params.project_root.as_deref())
                .map_err(|e| format!("resolve_core: {e}"))?;
            let mut core = core_handle.lock().unwrap();
            core.append_progress(&chapter_id, &params.line)
                .map_err(|e| {
                    tracing::warn!(error = ?e, "journal_append_progress failed");
                    e.to_string()
                })?
        };
        // HookWarning does not derive Serialize; format each warning as a string.
        let warning_strs: Vec<String> = warnings
            .iter()
            .map(|w| format!("{}: {}: {}", w.kind, w.section, w.hint))
            .collect();
        Ok(warning_strs.join("\n"))
    }

    /// Close an open chapter after validating all schema `requires` preconditions.
    ///
    /// Returns `"ok"` on success.
    #[tool(
        name = "journal_close_chapter",
        description = "Close an open chapter. \
                       Validates all schema requires preconditions before writing. \
                       Returns \"ok\" on success.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn journal_close_chapter(
        &self,
        Parameters(params): Parameters<JournalCloseChapterParams>,
    ) -> Result<String, String> {
        let chapter_id = journal_mcp_core::ChapterId(params.chapter_id);
        {
            // SAFETY: see journal_open_chapter
            let core_handle = self
                .resolve_core(params.project_root.as_deref())
                .map_err(|e| format!("resolve_core: {e}"))?;
            let mut core = core_handle.lock().unwrap();
            core.close_chapter(&chapter_id).map_err(|e| {
                tracing::warn!(error = ?e, "journal_close_chapter failed");
                e.to_string()
            })?
        };
        Ok("ok".to_string())
    }

    // -----------------------------------------------------------------------
    // Subtask 2: Crux #2 — schema 3 tool (must be independent entries)
    // -----------------------------------------------------------------------

    /// Load a ChapterSchema YAML literal into the SchemaRegistry L2 layer.
    ///
    /// Returns the registry key that was inserted (e.g. `"journal-mcp-canonical-v1"`).
    /// Repeated calls with the same YAML are idempotent (same key, same value).
    ///
    /// # Crux #2
    ///
    /// This is one of three independently-registered schema tools required by
    /// the Crux #2 constraint.  Must not be merged with `journal_schema_list`
    /// or `journal_schema_show`.
    #[tool(
        name = "journal_schema_load",
        description = "Load a ChapterSchema YAML literal into the SchemaRegistry L2 layer. \
                       Returns the registry key that was inserted (e.g. \"journal-mcp-canonical-v1\"). \
                       Idempotent: repeated calls with the same YAML overwrite with the same value.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn journal_schema_load(
        &self,
        Parameters(params): Parameters<JournalSchemaLoadParams>,
    ) -> Result<String, String> {
        let key = {
            // SAFETY: see journal_open_chapter
            let core_handle = self
                .resolve_core(params.project_root.as_deref())
                .map_err(|e| format!("resolve_core: {e}"))?;
            let mut core = core_handle.lock().unwrap();
            core.load_schema_yaml(&params.yaml).map_err(|e| {
                tracing::warn!(error = ?e, "journal_schema_load failed");
                e.to_string()
            })?
        }; // guard drops here — no await across the Mutex
        Ok(key)
    }

    /// List all available schema registry keys (built-in L1 + project-local L2).
    ///
    /// Returns a JSON array of key strings such as
    /// `["journal-mcp-canonical-v1", "madr-v1", "minimal-v1"]`.
    ///
    /// # Crux #2
    ///
    /// This is one of three independently-registered schema tools required by
    /// the Crux #2 constraint.
    #[tool(
        name = "journal_schema_list",
        description = "List all available schema registry keys (built-in L1 + project-local L2). \
                       Returns a JSON array of key strings.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn journal_schema_list(
        &self,
        Parameters(params): Parameters<JournalSchemaListParams>,
    ) -> Result<String, String> {
        let keys = {
            // SAFETY: see journal_open_chapter
            let core_handle = self
                .resolve_core(params.project_root.as_deref())
                .map_err(|e| format!("resolve_core: {e}"))?;
            let core = core_handle.lock().unwrap();
            core.schema_keys()
        }; // guard drops here
           // SAFETY: Vec<String> serialisation is infallible.
        let json =
            serde_json::to_string(&keys).expect("Vec<String> serialises to JSON without error");
        Ok(json)
    }

    /// Show the YAML specification of a given schema registry key.
    ///
    /// Returns the schema serialised as YAML text.  Returns an error string
    /// when the key does not exist in the registry.
    ///
    /// # Crux #2
    ///
    /// This is one of three independently-registered schema tools required by
    /// the Crux #2 constraint.
    #[tool(
        name = "journal_schema_show",
        description = "Show the YAML specification of a given schema registry key. \
                       Returns the schema as YAML text, or an error if the key is not found.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn journal_schema_show(
        &self,
        Parameters(params): Parameters<JournalSchemaShowParams>,
    ) -> Result<String, String> {
        let json = {
            // SAFETY: see journal_open_chapter
            let core_handle = self
                .resolve_core(params.project_root.as_deref())
                .map_err(|e| format!("resolve_core: {e}"))?;
            let core = core_handle.lock().unwrap();
            match core.schema_spec(&params.key) {
                Some(spec) => {
                    // ChapterSchema does not derive Serialize; build a JSON
                    // representation from its public accessors instead.
                    let sections: serde_json::Map<String, serde_json::Value> = spec
                        .sections()
                        .iter()
                        .map(|(k, v)| {
                            let section_json = serde_json::json!({
                                "required": v.required,
                                "evidence_required": v.evidence_required,
                                "description": v.description,
                            });
                            (k.clone(), section_json)
                        })
                        .collect();
                    let states: Vec<serde_json::Value> = spec
                        .states()
                        .iter()
                        .map(|s| serde_json::json!({ "id": s.id, "initial": s.initial, "terminal": s.terminal }))
                        .collect();
                    let transitions: Vec<serde_json::Value> = spec
                        .transitions()
                        .iter()
                        .map(|t| serde_json::json!({ "from": t.from, "to": t.to, "on": t.on }))
                        .collect();
                    let val = serde_json::json!({
                        "schema_id": spec.schema_id(),
                        "version": spec.version(),
                        "states": states,
                        "transitions": transitions,
                        "sections": sections,
                        "section_order": spec.section_order(),
                        "chapter_header": spec.chapter_header(),
                        "section_header": spec.section_header(),
                    });
                    // SAFETY: serde_json::Value always serialises to valid JSON.
                    serde_json::to_string_pretty(&val)
                        .expect("serde_json::Value serialises without error")
                }
                None => {
                    return Err(format!("schema not found: {}", params.key));
                }
            }
        }; // guard drops here
        Ok(json)
    }

    // -----------------------------------------------------------------------
    // Subtask 2: read 3 tool — tail / grep / chapter_list
    // -----------------------------------------------------------------------

    /// Fetch the last N chapters (default 10), newest first.
    ///
    /// Returns a JSON array of chapter objects.
    #[tool(
        name = "journal_tail",
        description = "Fetch the last N chapters (default 10), newest first. \
                       Returns a JSON array of chapter objects with metadata and events.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn journal_tail(
        &self,
        Parameters(params): Parameters<JournalTailParams>,
    ) -> Result<String, String> {
        let n = params.n.unwrap_or(10);
        let chapters = {
            // SAFETY: see journal_open_chapter
            let core_handle = self
                .resolve_core(params.project_root.as_deref())
                .map_err(|e| format!("resolve_core: {e}"))?;
            let core = core_handle.lock().unwrap();
            core.tail_chapters(n).map_err(|e| {
                tracing::warn!(error = ?e, n, "journal_tail failed");
                e.to_string()
            })?
        }; // guard drops here
        let values: Vec<serde_json::Value> = chapters.iter().map(chapter_replay_to_json).collect();
        // SAFETY: Vec<Value> serialisation is infallible.
        let json = serde_json::to_string(&values)
            .expect("Vec<serde_json::Value> serialises without error");
        Ok(json)
    }

    /// Search all chapters for events whose body payload contains a substring.
    ///
    /// Returns a JSON array of `{chapter_id, section_name, body}` objects for
    /// each matching `section_append` event.  An optional `since` / `until`
    /// filter (Unix epoch ms) restricts which chapters are scanned by their
    /// `opened_at` timestamp.
    #[tool(
        name = "journal_grep",
        description = "Search all chapter section bodies for a substring pattern. \
                       Optional since/until (Unix epoch ms) filter which chapters are scanned \
                       by their opened_at timestamp. \
                       Returns a JSON array of {chapter_id, section_name, body} matches.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn journal_grep(
        &self,
        Parameters(params): Parameters<JournalGrepParams>,
    ) -> Result<String, String> {
        // Strategy: call grep_chapters to get all pattern matches, then build
        // a set of allowed chapter IDs from chapter_ids(since) filtered by
        // `until`.  All within the Mutex guard scope (no await across the lock).
        let json = {
            // SAFETY: see journal_open_chapter
            let core_handle = self
                .resolve_core(params.project_root.as_deref())
                .map_err(|e| format!("resolve_core: {e}"))?;
            let core = core_handle.lock().unwrap();

            // 1. Collect chapter IDs that pass the `since` filter.
            let since_ids = core.chapter_ids(params.since).map_err(|e| {
                tracing::warn!(error = ?e, "journal_grep: chapter_ids failed");
                e.to_string()
            })?;

            // 2. If `until` is specified, further filter by fetching each chapter's
            //    opened_at via tail_chapters.  For simplicity, build an allowed-set
            //    of chapter_id strings.
            let allowed: std::collections::HashSet<String> = if params.until.is_some() {
                let until_ms = params.until.unwrap();
                let all = core.tail_chapters(usize::MAX).map_err(|e| {
                    tracing::warn!(error = ?e, "journal_grep: tail_chapters for until filter failed");
                    e.to_string()
                })?;
                // Intersect: chapter must be in since_ids AND opened_at <= until_ms
                let since_set: std::collections::HashSet<String> =
                    since_ids.iter().map(|id| id.0.clone()).collect();
                all.into_iter()
                    .filter(|r| {
                        r.meta.opened_at <= until_ms && since_set.contains(&r.meta.chapter_id.0)
                    })
                    .map(|r| r.meta.chapter_id.0)
                    .collect()
            } else {
                since_ids.into_iter().map(|id| id.0).collect()
            };

            // 3. Run pattern grep across all chapters, then filter by allowed set.
            let raw = core.grep_chapters(&params.pattern).map_err(|e| {
                tracing::warn!(error = ?e, pattern = %params.pattern, "journal_grep: grep_chapters failed");
                e.to_string()
            })?;

            let matches: Vec<serde_json::Value> = raw
                .into_iter()
                .filter(|(cid, _, _)| allowed.contains(&cid.0))
                .map(|(cid, section_name, body)| {
                    serde_json::json!({
                        "chapter_id": cid.0,
                        "section_name": section_name,
                        "body": body,
                    })
                })
                .collect();

            // SAFETY: Vec<Value> serialisation is infallible.
            serde_json::to_string(&matches)
                .expect("Vec<serde_json::Value> serialises without error")
        }; // guard drops here
        Ok(json)
    }

    /// List all chapters in the journal as a summary table.
    ///
    /// Returns a JSON array of chapter summary objects (Microsoft Decision Log
    /// format) with `chapter_id`, `schema_id`, `current_state`, `opened_at`,
    /// `closed_at`, `decided_summary`, and `link` fields.
    #[tool(
        name = "journal_chapter_list",
        description = "List all chapters as a summary table (Microsoft Decision Log format). \
                       Returns a JSON array with chapter_id, schema_id, current_state, \
                       opened_at, closed_at, decided_summary, and link fields.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn journal_chapter_list(
        &self,
        Parameters(params): Parameters<JournalChapterListParams>,
    ) -> Result<String, String> {
        let rows = {
            // SAFETY: see journal_open_chapter
            let core_handle = self
                .resolve_core(params.project_root.as_deref())
                .map_err(|e| format!("resolve_core: {e}"))?;
            let core = core_handle.lock().unwrap();
            // tail_chapters with a large n gives all chapters (newest first)
            let chapters = core.tail_chapters(usize::MAX).map_err(|e| {
                tracing::warn!(error = ?e, "journal_chapter_list: tail_chapters failed");
                e.to_string()
            })?;

            // Apply pagination: skip(offset).take(limit).
            // - offset default = 0 (no skipping)
            // - limit default = usize::MAX (return all remaining)
            // Both omitted = full list (backward-compatible with pre-pagination
            // behaviour).  offset >= total yields an empty Vec, not an error.
            let paginated = Self::paginate(chapters, params.offset, params.limit);

            paginated
                .into_iter()
                .map(|replay| {
                    let decided_summary = replay
                        .events
                        .iter()
                        .filter(|e| {
                            e.event_type == "section_append"
                                && e.section_name.as_deref() == Some("Decided")
                        })
                        .find_map(|e| {
                            serde_json::from_str::<serde_json::Value>(&e.payload)
                                .ok()
                                .and_then(|v| {
                                    v.get("body")
                                        .and_then(|b| b.as_str())
                                        .map(|s| s.lines().next().unwrap_or("").to_owned())
                                })
                        })
                        .unwrap_or_default();

                    let link = format!("{}#decided", replay.meta.chapter_id.0);

                    ChapterListRow {
                        chapter_id: replay.meta.chapter_id.0.clone(),
                        schema_id: replay.meta.schema_id.clone(),
                        current_state: replay.meta.current_state.clone(),
                        opened_at: replay.meta.opened_at,
                        closed_at: replay.meta.closed_at,
                        decided_summary,
                        link,
                    }
                })
                .collect::<Vec<_>>()
        }; // guard drops here
        let json =
            serde_json::to_string(&rows).expect("Vec<ChapterListRow> serialises without error");
        Ok(json)
    }

    // -----------------------------------------------------------------------
    // Subtask 3: remaining 5 tools — open_chapters / progress_of / projection 3
    // ST7 adds journal_import as the 16th tool — all in this single #[tool_router] block.
    // -----------------------------------------------------------------------

    /// List all chapters that are still open (closed_at IS NULL).
    ///
    /// Returns a JSON array of chapter ID strings for chapters that have not
    /// yet been closed.  Useful for resuming work on unfinished entries.
    #[tool(
        name = "journal_open_chapters",
        description = "List all chapters that are still open (closed_at IS NULL). \
                       Returns a JSON array of chapter ID strings.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn journal_open_chapters(
        &self,
        Parameters(params): Parameters<JournalOpenChaptersParams>,
    ) -> Result<String, String> {
        let ids = {
            // SAFETY: see journal_open_chapter
            let core_handle = self
                .resolve_core(params.project_root.as_deref())
                .map_err(|e| format!("resolve_core: {e}"))?;
            let core = core_handle.lock().unwrap();
            core.open_chapter_ids().map_err(|e| {
                tracing::warn!(error = ?e, "journal_open_chapters failed");
                e.to_string()
            })?
        }; // guard drops here
        let id_strs: Vec<String> = ids.into_iter().map(|id| id.0).collect();
        // SAFETY: Vec<String> serialisation is infallible.
        let json =
            serde_json::to_string(&id_strs).expect("Vec<String> serialises to JSON without error");
        Ok(json)
    }

    /// Read all body lines from the `Progress` section of a specific chapter.
    ///
    /// Returns a JSON array of progress body strings in append order
    /// (earliest first).  Returns an empty array when no Progress entries exist.
    #[tool(
        name = "journal_progress_of",
        description = "Read the Progress section of a specific chapter. \
                       Returns a JSON array of progress body strings in append order.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn journal_progress_of(
        &self,
        Parameters(params): Parameters<JournalProgressOfParams>,
    ) -> Result<String, String> {
        let chapter_id = journal_mcp_core::ChapterId(params.chapter_id);
        let entries = {
            // SAFETY: see journal_open_chapter
            let core_handle = self
                .resolve_core(params.project_root.as_deref())
                .map_err(|e| format!("resolve_core: {e}"))?;
            let core = core_handle.lock().unwrap();
            core.progress_of(&chapter_id).map_err(|e| {
                tracing::warn!(error = ?e, chapter_id = %chapter_id, "journal_progress_of failed");
                e.to_string()
            })?
        }; // guard drops here
           // SAFETY: Vec<String> serialisation is infallible.
        let json =
            serde_json::to_string(&entries).expect("Vec<String> serialises to JSON without error");
        Ok(json)
    }

    /// Attach a named projection (currently only `"file"` is supported, and it
    /// is auto-attached at server startup).
    ///
    /// Attaching `"file"` is idempotent — the server returns a success message
    /// since the `FileProjection` is always active from startup.  Requesting
    /// any other name returns an error.
    #[tool(
        name = "journal_projection_attach",
        description = "Attach a named projection. Currently only 'file' is supported \
                       and is always auto-attached at startup (idempotent). \
                       Other names return an error.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn journal_projection_attach(
        &self,
        Parameters(params): Parameters<JournalProjectionAttachParams>,
    ) -> Result<String, String> {
        match params.name.as_str() {
            "file" => {
                tracing::info!(
                    "journal_projection_attach: 'file' projection already auto-attached at startup"
                );
                Ok("file projection already auto-attached at startup".to_string())
            }
            other => {
                tracing::warn!(
                    name = other,
                    "journal_projection_attach: unknown projection name"
                );
                Err(format!("projection not found: {other}"))
            }
        }
    }

    /// Detach a named projection.
    ///
    /// **Not yet supported** in this release (first cut scope).  The tool
    /// entry is registered to satisfy Crux #1 (15 tool full registration)
    /// but always returns an error.  Detach support is planned for ST7
    /// (`docs/design.md §10 Step 7`).
    #[tool(
        name = "journal_projection_detach",
        description = "Detach a named projection. \
                       NOT YET SUPPORTED in this release (first cut, see design §10 Step 7). \
                       Always returns an error.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn journal_projection_detach(
        &self,
        _params: Parameters<JournalProjectionDetachParams>,
    ) -> Result<String, String> {
        // Crux #1: this tool entry must be registered even though it is unsupported.
        // The error signals the caller that detach will be supported in ST7.
        tracing::warn!(
            "journal_projection_detach: not yet supported (see docs/design.md §10 Step 7)"
        );
        Err(
            "projection detach is not yet supported (first cut scope, see docs/design.md §10 Step 7)"
                .to_string(),
        )
    }

    /// Rebuild a named projection by replaying the full EventLog.
    ///
    /// Iterates all closed chapters in the EventLog and calls the projection's
    /// `rebuild_chapter` for each one.  Useful after a projection output file
    /// has been lost or corrupted.
    ///
    /// # Arguments
    ///
    /// * `name` — the stable name of the projection to rebuild (e.g. `"file"`).
    ///   Must match a projection registered at startup.
    /// * `output_path` — optional per-call output path for a one-shot rebuild.
    ///   Only meaningful when `name == "file"`.
    #[tool(
        name = "journal_projection_rebuild",
        description = "Rebuild a named projection by replaying the full EventLog. \
                       Calls rebuild_chapter for every closed chapter. \
                       Use 'file' to rebuild the journal.md output. \
                       Optional output_path overrides the default attached path for a one-shot \
                       rebuild (file projection only; attached projection is unchanged).",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn journal_projection_rebuild(
        &self,
        Parameters(params): Parameters<JournalProjectionRebuildParams>,
    ) -> Result<String, String> {
        // Determine the effective project_root for path resolution.
        let effective_root: PathBuf = if let Some(pr) = &params.project_root {
            let p = PathBuf::from(pr);
            std::fs::canonicalize(&p).unwrap_or(p)
        } else {
            self.project_root.clone()
        };

        // per-call output_path override — one-shot rebuild only.
        if let Some(ref raw_output) = params.output_path {
            if params.name == "file" {
                // Resolve the output path.
                let output_path = {
                    let p = PathBuf::from(raw_output);
                    if p.is_absolute() {
                        p
                    } else {
                        effective_root.join(&p)
                    }
                };

                // Ensure parent directory exists.
                if let Some(parent) = output_path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("failed to create output_path parent dir: {e}"))?;
                }

                // Build a temporary FileProjection targeting output_path, then
                // replay all closed chapters into it (one-shot, attached
                // projection is untouched).
                let core_handle = self
                    .resolve_core(params.project_root.as_deref())
                    .map_err(|e| format!("resolve_core: {e}"))?;
                let core = core_handle.lock().unwrap();

                // Fetch registry Arc from the core via a temporary FileProjection wrapper.
                // We need a SchemaRegistry Arc; reconstruct from the effective root.
                let registry =
                    journal_mcp_core::SchemaRegistry::with_project_local(&effective_root)
                        .map_err(|e| format!("SchemaRegistry::with_project_local: {e}"))?;
                let registry_arc = std::sync::Arc::new(registry);

                let mut temp_proj =
                    journal_mcp_core::FileProjection::new(output_path.clone(), registry_arc);

                // Replay all closed chapters into the temp projection.
                let all_chapters = core.tail_chapters(usize::MAX).map_err(|e| {
                    tracing::warn!(error = ?e, "journal_projection_rebuild: tail_chapters failed");
                    e.to_string()
                })?;
                for replay in &all_chapters {
                    if replay.meta.closed_at.is_none() {
                        continue; // skip open chapters
                    }
                    use journal_mcp_core::JournalProjection as _;
                    temp_proj.rebuild_chapter(replay).map_err(|e| {
                        tracing::warn!(error = ?e, chapter_id = %replay.meta.chapter_id, "one-shot rebuild_chapter failed");
                        e.to_string()
                    })?;
                }

                tracing::info!(
                    output_path = ?output_path,
                    "journal_projection_rebuild: one-shot rebuild complete"
                );
                return Ok(format!(
                    "projection 'file' rebuilt (one-shot) to {}",
                    output_path.display()
                ));
            } else {
                // output_path is only meaningful for name == "file"; warn and fall through.
                tracing::warn!(
                    name = %params.name,
                    "journal_projection_rebuild: output_path is only applicable for name='file'; \
                     ignoring and using default rebuild"
                );
            }
        }

        // Default path: rebuild the named attached projection.
        {
            // SAFETY: see journal_open_chapter
            let core_handle = self
                .resolve_core(params.project_root.as_deref())
                .map_err(|e| format!("resolve_core: {e}"))?;
            let mut core = core_handle.lock().unwrap();
            core.rebuild_projection(&params.name).map_err(|e| {
                tracing::warn!(error = ?e, name = %params.name, "journal_projection_rebuild failed");
                e.to_string()
            })?;
        } // guard drops here — no await across the Mutex
        Ok(format!("projection '{}' rebuilt", params.name))
    }

    /// Import chapters from an existing markdown file into the journal.
    ///
    /// Parses the file using journal-mcp-canonical-v1 rules (h2=chapter, h3=section),
    /// inserts all chapters in one atomic SQLite transaction, and returns a JSON
    /// array of the chapter IDs that were imported.
    ///
    /// If any `chapter_id` already exists the entire batch is rolled back and an
    /// error is returned (no partial state).  Projection rebuild is **not**
    /// triggered automatically — invoke `journal_projection_rebuild` explicitly
    /// after import if rendering is needed (Crux #1 explicit-only render policy).
    #[tool(
        name = "journal_import",
        description = "Import chapters from a markdown file (journal-mcp-canonical-v1: h2=chapter, h3=section). \
                       Atomic batch insert — any chapter_id collision rolls back the entire batch. \
                       Returns JSON array of imported chapter IDs. \
                       Does NOT trigger projection rebuild (call journal_projection_rebuild explicitly).",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn journal_import(
        &self,
        Parameters(params): Parameters<JournalImportParams>,
    ) -> Result<String, String> {
        let imported = {
            // SAFETY: see journal_open_chapter
            let core_handle = self
                .resolve_core(params.project_root.as_deref())
                .map_err(|e| format!("resolve_core: {e}"))?;
            let mut core = core_handle.lock().unwrap();
            let path = std::path::PathBuf::from(&params.path);
            core.import_chapter(&path).map_err(|e| {
                tracing::warn!(error = ?e, path = %params.path, "journal_import failed");
                e.to_string()
            })?
        }; // guard drops here — no await across the Mutex
        let ids: Vec<&str> = imported.iter().map(|id| id.0.as_str()).collect();
        Ok(serde_json::to_string(&ids).unwrap_or_else(|_| "[]".to_string()))
    }

    /// Return server runtime state for diagnostic purposes.
    ///
    /// Read-only tool. Returns resolved paths, schema list, server version,
    /// and startup timestamp. Useful for confirming which database the server
    /// is using and diagnosing path resolution issues.
    #[tool(
        name = "journal_info",
        description = "Return server runtime state (paths, schemas, version, startup time). \
                       Read-only diagnostic tool; no side effects.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn journal_info(&self) -> Result<String, String> {
        let db_path = self.db_path.clone();
        let db_exists = db_path.exists();
        let wal_path = {
            let mut p = db_path.clone().into_os_string();
            p.push("-wal");
            PathBuf::from(p)
        };
        let shm_path = {
            let mut p = db_path.clone().into_os_string();
            p.push("-shm");
            PathBuf::from(p)
        };

        let available_schemas: Vec<String> = {
            let core = self.core.lock().unwrap();
            core.schema_keys()
        };

        let result = JournalInfoResult {
            project_root: self.project_root.clone(),
            db_path,
            db_exists,
            wal_path,
            shm_path,
            schema_registry_path: self.schema_registry_path.clone(),
            available_schemas,
            version: env!("CARGO_PKG_VERSION").to_string(),
            startup_time: self.started_at.clone(),
            env_journal_project_root: self.env_journal_project_root.clone(),
            file_projection_path: self.file_projection_path.clone(),
        };

        serde_json::to_string(&result).map_err(|e| {
            tracing::warn!(error = ?e, "journal_info serialization failed");
            e.to_string()
        })
    }
}

// ---------------------------------------------------------------------------
// Local helpers (MCP-layer serialisation utilities)
// ---------------------------------------------------------------------------

/// Convert a [`journal_mcp_core::ChapterReplay`] to a `serde_json::Value`.
///
/// `ChapterReplay` (and `ChapterMeta` / `EventRow`) do not derive `Serialize`
/// in the `journal` crate (to keep the library layer clean).  This helper
/// provides the MCP-layer projection without polluting the library.
fn chapter_replay_to_json(replay: &journal_mcp_core::ChapterReplay) -> serde_json::Value {
    let events: Vec<serde_json::Value> = replay
        .events
        .iter()
        .map(|e| {
            serde_json::json!({
                "event_id": e.event_id.0,
                "event_type": e.event_type,
                "section_name": e.section_name,
                "payload": e.payload,
                "created_at": e.created_at,
            })
        })
        .collect();
    serde_json::json!({
        "chapter_id": replay.meta.chapter_id.0,
        "schema_id": replay.meta.schema_id,
        "current_state": replay.meta.current_state,
        "opened_at": replay.meta.opened_at,
        "closed_at": replay.meta.closed_at,
        "events": events,
    })
}

// ---------------------------------------------------------------------------
// ServerHandler — Crux #1: #[tool_handler] macro wires tool_router dispatch
// ---------------------------------------------------------------------------

/// `ServerHandler` implementation for `JournalMcpServer`.
///
/// The `#[tool_handler]` macro generates the `call_tool` dispatch that routes
/// MCP wire calls to the correct `#[tool_router]` method.  Only `get_info`
/// is manually implemented here; all other `ServerHandler` methods keep their
/// default no-op implementations.
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

// ---------------------------------------------------------------------------
// Entry point — Crux #3: stdio transport fixed wiring
// ---------------------------------------------------------------------------

/// Main entry point.
///
/// Resolves `JOURNAL_PROJECT_ROOT` (defaults to `std::env::current_dir()`),
/// initialises a [`JournalMcpServer`], and serves over **stdio transport**
/// as required by Crux #3.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    let project_root = std::env::var("JOURNAL_PROJECT_ROOT")
        .map(PathBuf::from)
        // SAFETY: current_dir() can only fail if the process has no CWD (e.g. the
        // directory was deleted while the process was running).  This is a fatal
        // startup condition; panicking here is intentional and safe.
        .unwrap_or_else(|_| std::env::current_dir().expect("cwd accessible at startup"));

    tracing::info!(?project_root, "journal-mcp starting");

    let server = JournalMcpServer::new(project_root)?;

    // Crux #3: stdio transport — tokio::io::stdin / stdout wrapper.
    // Must not be replaced with TCP / HTTP / unix socket.
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Inline integration tests (file count = 5 maintained by keeping tests here)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `JournalMcpServer` backed by a temporary directory.
    ///
    /// FileProjection is not attached (`attach_file_projection = false`) so that
    /// tests do not touch the real filesystem outside the `TempDir` (Crux #3).
    fn make_server(tmp: &tempfile::TempDir) -> JournalMcpServer {
        JournalMcpServer::new_with_config(tmp.path().to_path_buf(), false)
            // SAFETY: TempDir is kept alive by caller; new_with_config() creates workspace/ subdir.
            .expect("JournalMcpServer::new_with_config should succeed in temp dir")
    }

    /// T1 (property) — four lifecycle tools are registered in the tool_router.
    ///
    /// Verifies Crux #1: all tools are registered in the single `#[tool_router]`
    /// block on `JournalMcpServer`.
    #[test]
    fn test_subtask1_four_lifecycle_tools_registered() {
        // SAFETY: TempDir::new() panics only if the OS cannot create a temp dir.
        let tmp = tempfile::TempDir::new().expect("TempDir::new should succeed");
        let server = make_server(&tmp);
        let tools = server.tool_router.list_all();
        let tool_names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        assert!(
            tool_names.contains(&"journal_open_chapter"),
            "journal_open_chapter must be registered; got: {tool_names:?}"
        );
        assert!(
            tool_names.contains(&"journal_append_section"),
            "journal_append_section must be registered; got: {tool_names:?}"
        );
        assert!(
            tool_names.contains(&"journal_append_progress"),
            "journal_append_progress must be registered; got: {tool_names:?}"
        );
        assert!(
            tool_names.contains(&"journal_close_chapter"),
            "journal_close_chapter must be registered; got: {tool_names:?}"
        );
    }

    /// T2 (boundary) — `JournalMcpServer::new` succeeds with an empty temp dir.
    #[test]
    fn test_server_new_creates_workspace_dir() {
        let tmp = tempfile::TempDir::new().expect("TempDir::new should succeed");
        let workspace = tmp.path().join("workspace");
        assert!(
            !workspace.exists(),
            "workspace should not exist before new()"
        );
        let _server = make_server(&tmp);
        assert!(workspace.exists(), "workspace should be created by new()");
    }

    /// T3 (error path) — tool_router now returns exactly 17 tools after ST7.
    ///
    /// Updated from "exactly 16 tools" (ST7) to "exactly 17 tools" (journal_info)
    /// by adding `journal_info` as the 17th tool.
    ///
    /// Verifies Crux #1: all 17 tools are wired into the single `#[tool_router]` block.
    #[test]
    fn test_st7_exactly_seventeen_tools() {
        let tmp = tempfile::TempDir::new().expect("TempDir::new should succeed");
        let server = make_server(&tmp);
        let count = server.tool_router.list_all().len();
        assert_eq!(
            count, 17,
            "ST7 tool_router should have exactly 17 tools (Crux #1), got {count}"
        );
    }

    /// T1 (property) — Crux #2: schema 3 tool registered as independent entries.
    ///
    /// Verifies that `journal_schema_load`, `journal_schema_list`, and
    /// `journal_schema_show` are each registered as separate MCP tool entries
    /// in the tool_router (Crux #2: must not be merged into 1 tool).
    #[test]
    fn test_subtask2_crux2_schema_three_tools_independent() {
        let tmp = tempfile::TempDir::new().expect("TempDir::new should succeed");
        let server = make_server(&tmp);
        let tools = server.tool_router.list_all();
        let tool_names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();

        // Crux #2: each must be an independent tool entry
        assert!(
            tool_names.contains(&"journal_schema_load"),
            "journal_schema_load must be registered as an independent tool; got: {tool_names:?}"
        );
        assert!(
            tool_names.contains(&"journal_schema_list"),
            "journal_schema_list must be registered as an independent tool; got: {tool_names:?}"
        );
        assert!(
            tool_names.contains(&"journal_schema_show"),
            "journal_schema_show must be registered as an independent tool; got: {tool_names:?}"
        );
    }

    /// T1 (property) — all 10 ST1+ST2 tools are registered.
    #[test]
    fn test_subtask2_ten_tools_registered() {
        let tmp = tempfile::TempDir::new().expect("TempDir::new should succeed");
        let server = make_server(&tmp);
        let tools = server.tool_router.list_all();
        let tool_names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();

        // ST1 lifecycle tools (verified separately in test_subtask1_*)
        for name in &[
            "journal_open_chapter",
            "journal_append_section",
            "journal_append_progress",
            "journal_close_chapter",
        ] {
            assert!(
                tool_names.contains(name),
                "tool {name} should be registered; got: {tool_names:?}"
            );
        }
        // ST2 schema tools (Crux #2)
        for name in &[
            "journal_schema_load",
            "journal_schema_list",
            "journal_schema_show",
        ] {
            assert!(
                tool_names.contains(name),
                "tool {name} should be registered; got: {tool_names:?}"
            );
        }
        // ST2 read tools
        for name in &["journal_tail", "journal_grep", "journal_chapter_list"] {
            assert!(
                tool_names.contains(name),
                "tool {name} should be registered; got: {tool_names:?}"
            );
        }
    }

    /// T2 (boundary) — schema_list returns at least 3 built-in schema keys.
    ///
    /// Verifies that `journal_schema_list` returns meaningful data (at least
    /// the 3 built-in schemas: journal-mcp-canonical-v1, madr-v1, minimal-v1).
    #[test]
    fn test_schema_list_returns_builtins() {
        let tmp = tempfile::TempDir::new().expect("TempDir::new should succeed");
        let server = make_server(&tmp);
        let core = server.core.lock().expect("Mutex should not be poisoned");
        let keys = core.schema_keys();
        assert!(
            keys.len() >= 3,
            "schema_keys should return at least 3 built-in keys, got: {keys:?}"
        );
        assert!(
            keys.iter().any(|k| k.contains("journal-mcp-canonical")),
            "built-in journal-mcp-canonical should be in schema_keys; got: {keys:?}"
        );
    }

    /// T3 (error path) — schema_spec returns None for an unknown key.
    #[test]
    fn test_schema_spec_unknown_key_returns_none() {
        let tmp = tempfile::TempDir::new().expect("TempDir::new should succeed");
        let server = make_server(&tmp);
        let core = server.core.lock().expect("Mutex should not be poisoned");
        assert!(
            core.schema_spec("nonexistent-schema-v999").is_none(),
            "schema_spec should return None for an unknown key"
        );
    }

    // -----------------------------------------------------------------------
    // ST3 integration tests — Crux #1 final: 15 tool full registration assert
    // -----------------------------------------------------------------------

    /// Canonical spelling of all 17 MCP tools in the tool_router (ST7 final).
    ///
    /// This constant is the authoritative list.  Changing this list is a
    /// Crux #1 or Crux #2 violation and requires human review.
    const EXPECTED_TOOLS: &[&str] = &[
        // ST1: chapter lifecycle (4)
        "journal_open_chapter",
        "journal_append_section",
        "journal_append_progress",
        "journal_close_chapter",
        // ST2: schema tools — Crux #2 requires each as an independent entry (3)
        "journal_schema_load",
        "journal_schema_list",
        "journal_schema_show",
        // ST2: read tools (3)
        "journal_tail",
        "journal_grep",
        "journal_chapter_list",
        // ST3: remaining tools (5)
        "journal_open_chapters",
        "journal_progress_of",
        "journal_projection_attach",
        "journal_projection_detach",
        "journal_projection_rebuild",
        // ST7: import tool (1)
        "journal_import",
        // journal_info: diagnostic tool (1)
        "journal_info",
    ];

    /// T1 (property) — Crux #1 final: all 17 tools are registered in the
    /// single `#[tool_router] impl JournalMcpServer` block.
    ///
    /// This is the primary acceptance test for ST7 as a whole.
    #[test]
    fn test_all_seventeen_tools_registered() {
        let tmp = tempfile::TempDir::new().expect("TempDir::new should succeed");
        let server = make_server(&tmp);
        let tools = server.tool_router.list_all();
        let tool_names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        assert_eq!(
            tool_names.len(),
            17,
            "exactly 17 tools must be registered (Crux #1 ST7); got: {tool_names:?}"
        );
        for &name in EXPECTED_TOOLS {
            assert!(
                tool_names.contains(&name),
                "tool '{name}' must be registered (Crux #1); registered: {tool_names:?}"
            );
        }
    }

    /// T1 (property) — Crux #2 preserved: schema 3 tools remain independent
    /// entries after ST3 additions.
    #[test]
    fn test_crux2_schema_tools_still_independent_after_st3() {
        let tmp = tempfile::TempDir::new().expect("TempDir::new should succeed");
        let server = make_server(&tmp);
        let tools = server.tool_router.list_all();
        let tool_names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        for &name in &[
            "journal_schema_load",
            "journal_schema_list",
            "journal_schema_show",
        ] {
            assert!(
                tool_names.contains(&name),
                "Crux #2: schema tool '{name}' must remain an independent entry; \
                 registered: {tool_names:?}"
            );
        }
    }

    /// T2 (boundary) — `journal_projection_detach` tool entry exists even though
    /// the handler always returns an error (Crux #1: tool entry != handler success).
    #[test]
    fn test_projection_detach_tool_entry_exists_crux1() {
        let tmp = tempfile::TempDir::new().expect("TempDir::new should succeed");
        let server = make_server(&tmp);
        let tools = server.tool_router.list_all();
        let tool_names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        assert!(
            tool_names.contains(&"journal_projection_detach"),
            "Crux #1: journal_projection_detach tool entry must be registered \
             even though it returns unsupported; registered: {tool_names:?}"
        );
    }

    /// T2 (boundary) — `journal_open_chapters` returns a JSON array (empty
    /// when no chapters are open on a fresh database).
    #[test]
    fn test_open_chapters_empty_on_fresh_db() {
        let tmp = tempfile::TempDir::new().expect("TempDir::new should succeed");
        let server = make_server(&tmp);
        let core = server.core.lock().expect("Mutex should not be poisoned");
        let ids = core
            .open_chapter_ids()
            .expect("open_chapter_ids should succeed on fresh db");
        assert!(
            ids.is_empty(),
            "fresh db should have no open chapters, got: {ids:?}"
        );
    }

    /// T3 (error path) — `journal_progress_of` for an unknown chapter returns
    /// an error from the EventLog layer.
    #[test]
    fn test_progress_of_unknown_chapter_returns_error() {
        let tmp = tempfile::TempDir::new().expect("TempDir::new should succeed");
        let server = make_server(&tmp);
        let core = server.core.lock().expect("Mutex should not be poisoned");
        let result = core.progress_of(&journal_mcp_core::ChapterId("nonexistent-id".to_string()));
        assert!(
            result.is_err(),
            "progress_of for a nonexistent chapter should return Err"
        );
    }

    // -----------------------------------------------------------------------
    // ST7/ST3 isolation tests — new_with_config / TempDir-only path guarantee
    // -----------------------------------------------------------------------

    /// T1 (property) — `new_with_config(_, false)` succeeds and returns a
    /// working server with no FileProjection attached.
    ///
    /// Verifies Crux #3: test-only constructor must succeed and all file paths
    /// must remain inside the TempDir (no real journal.md touched).
    #[test]
    fn test_new_with_config_no_fp_succeeds() {
        let tmp = tempfile::TempDir::new().expect("TempDir::new should succeed");
        let result = JournalMcpServer::new_with_config(tmp.path().to_path_buf(), false);
        assert!(
            result.is_ok(),
            "new_with_config(_, false) should return Ok; got: {:?}",
            result.err()
        );
        // The workspace directory must have been created inside TempDir.
        let workspace = tmp.path().join("workspace");
        assert!(
            workspace.exists(),
            "workspace dir should be created inside TempDir by new_with_config"
        );
        // Default FileProjection output = <project_root>/journal.md (root).
        // When attach_file_projection=false, journal.md must NOT be created at root
        // (no projection is attached, no file should be written).
        let root_journal_md = tmp.path().join("journal.md");
        assert!(
            !root_journal_md.exists(),
            "journal.md must not be created when attach_file_projection=false; \
             path: {root_journal_md:?}"
        );
        // workspace/journal.md (old default, pre-v0.3.0) also must not be created.
        let workspace_journal_md = tmp.path().join("workspace").join("journal.md");
        assert!(
            !workspace_journal_md.exists(),
            "workspace/journal.md must not be created; path: {workspace_journal_md:?}"
        );
    }

    /// T2 (boundary) — `new_with_config(_, true)` attaches FileProjection and
    /// creates `journal.md` (root) inside TempDir on first rebuild.
    ///
    /// When `attach_file_projection = true` the projection is wired up but the
    /// file is only written on explicit `journal_projection_rebuild`.  Verifies
    /// the server can be constructed successfully with projection attached.
    #[test]
    fn test_new_with_config_with_fp_constructs_ok() {
        let tmp = tempfile::TempDir::new().expect("TempDir::new should succeed");
        let result = JournalMcpServer::new_with_config(tmp.path().to_path_buf(), true);
        assert!(
            result.is_ok(),
            "new_with_config(_, true) should return Ok; got: {:?}",
            result.err()
        );
    }

    /// T3 (error path) — `new_with_config` propagates an error when the project
    /// root cannot have its workspace subdir created (invalid nested path).
    ///
    /// Uses a path that cannot be created because its parent is a file, not
    /// a directory, triggering `std::fs::create_dir_all` failure.
    #[test]
    fn test_new_with_config_returns_err_on_bad_root() {
        let tmp = tempfile::TempDir::new().expect("TempDir::new should succeed");
        // Create a regular file where `workspace` would need to be a directory.
        let blocker = tmp.path().join("workspace");
        std::fs::write(&blocker, b"blocking file").expect("write should succeed");
        // Now attempt to use `workspace/subpath` as the project root — the
        // `workspace` segment is a file, so `create_dir_all` of its "workspace"
        // child must fail.
        let nested_root = blocker.join("subpath");
        let result = JournalMcpServer::new_with_config(nested_root, false);
        assert!(
            result.is_err(),
            "new_with_config should return Err when workspace dir cannot be created"
        );
    }

    // -----------------------------------------------------------------------
    // Per-call project_root override tests (multi-project workflow support)
    // -----------------------------------------------------------------------

    /// T1 (property) — `resolve_core(None)` returns the startup-time default
    /// core handle (Arc::ptr_eq).
    #[test]
    fn test_resolve_core_default_returns_startup_core() {
        let tmp = tempfile::TempDir::new().expect("TempDir::new should succeed");
        let server = make_server(&tmp);
        let handle = server
            .resolve_core(None)
            .expect("resolve_core(None) should succeed");
        assert!(
            Arc::ptr_eq(&handle, &server.core),
            "resolve_core(None) must return the startup-time default core"
        );
    }

    /// T2 (boundary) — `resolve_core(Some(path))` rooted at a different project
    /// lazily creates a separate `JournalCore` and a `.journal.db` file at
    /// `{path}/workspace/.journal.db`.
    #[test]
    fn test_resolve_core_override_creates_separate_db() {
        let tmp_default = tempfile::TempDir::new().expect("TempDir::new should succeed");
        let tmp_other = tempfile::TempDir::new().expect("TempDir::new should succeed");
        let server = make_server(&tmp_default);

        let other_path = tmp_other
            .path()
            .to_str()
            .expect("temp dir path should be UTF-8");
        let handle = server
            .resolve_core(Some(other_path))
            .expect("resolve_core(Some(other)) should succeed");

        assert!(
            !Arc::ptr_eq(&handle, &server.core),
            "override must return a distinct core handle, not the default core"
        );

        let other_db = tmp_other.path().join("workspace").join(".journal.db");
        assert!(
            other_db.exists(),
            "override must create {{path}}/workspace/.journal.db; checked: {other_db:?}"
        );
    }

    /// T3 (property) — Repeated `resolve_core(Some(same_path))` returns the
    /// **cached** handle (same Arc pointer), not a fresh instance.
    #[test]
    fn test_resolve_core_override_caches_handle() {
        let tmp_default = tempfile::TempDir::new().expect("TempDir::new should succeed");
        let tmp_other = tempfile::TempDir::new().expect("TempDir::new should succeed");
        let server = make_server(&tmp_default);
        let path = tmp_other
            .path()
            .to_str()
            .expect("temp dir path should be UTF-8");

        let h1 = server
            .resolve_core(Some(path))
            .expect("first call should succeed");
        let h2 = server
            .resolve_core(Some(path))
            .expect("second call should succeed");
        assert!(
            Arc::ptr_eq(&h1, &h2),
            "repeated resolve_core(Some(same path)) must return the cached handle"
        );
    }

    /// T4 (canonical short-circuit) — Path that canonicalizes to the default
    /// project_root returns the default core (no extra cache entry).
    #[test]
    fn test_resolve_core_canonical_matches_default() {
        let tmp_default = tempfile::TempDir::new().expect("TempDir::new should succeed");
        let server = make_server(&tmp_default);
        let default_path_str = tmp_default
            .path()
            .to_str()
            .expect("temp dir path should be UTF-8");

        let handle = server
            .resolve_core(Some(default_path_str))
            .expect("resolve_core should succeed");
        assert!(
            Arc::ptr_eq(&handle, &server.core),
            "Path that canonicalizes to default project_root must return the default core"
        );
    }

    // -----------------------------------------------------------------------
    // journal_chapter_list pagination tests (limit / offset)
    // -----------------------------------------------------------------------

    /// T1 (property) — `paginate(_, None, None)` returns the full input
    /// unchanged (backward-compat with pre-pagination behaviour).
    #[test]
    fn test_paginate_omitted_returns_all() {
        let v = vec![1, 2, 3, 4, 5];
        let r = JournalMcpServer::paginate(v.clone(), None, None);
        assert_eq!(r, v);
    }

    /// T2 (boundary) — `paginate(_, None, Some(N))` returns the first N items.
    #[test]
    fn test_paginate_limit_only() {
        let v = vec![1, 2, 3, 4, 5];
        let r = JournalMcpServer::paginate(v, None, Some(2));
        assert_eq!(r, vec![1, 2]);
    }

    /// T3 (boundary) — `paginate(_, Some(K), None)` skips K items and returns
    /// the rest.
    #[test]
    fn test_paginate_offset_only() {
        let v = vec![1, 2, 3, 4, 5];
        let r = JournalMcpServer::paginate(v, Some(2), None);
        assert_eq!(r, vec![3, 4, 5]);
    }

    /// T4 (property) — Both `offset` and `limit` compose: skip K then take N.
    #[test]
    fn test_paginate_limit_and_offset() {
        let v = vec![1, 2, 3, 4, 5];
        let r = JournalMcpServer::paginate(v, Some(1), Some(2));
        assert_eq!(r, vec![2, 3]);
    }

    /// T5 (error path) — `offset >= len` yields an empty `Vec`, not an error.
    #[test]
    fn test_paginate_offset_overflow_yields_empty() {
        let v: Vec<i32> = vec![1, 2, 3];
        let r = JournalMcpServer::paginate(v, Some(10), Some(5));
        assert!(
            r.is_empty(),
            "offset >= len must yield an empty Vec; got: {r:?}"
        );
    }

    /// T6 (boundary) — `offset = 0` is equivalent to omitting `offset`.
    #[test]
    fn test_paginate_offset_zero_same_as_none() {
        let v = vec![1, 2, 3];
        let with_none = JournalMcpServer::paginate(v.clone(), None, Some(2));
        let with_zero = JournalMcpServer::paginate(v, Some(0), Some(2));
        assert_eq!(with_none, with_zero);
    }

    /// Verify that `JournalInfoResult` has all 10 expected fields and that all
    /// path-typed fields are absolute paths.
    #[test]
    fn test_journal_info_return_shape() {
        let tmp = tempfile::TempDir::new().expect("TempDir::new should succeed");
        // Canonicalize the TempDir path (macOS: /var -> /private/var).
        let canonical_root =
            std::fs::canonicalize(tmp.path()).unwrap_or_else(|_| tmp.path().to_path_buf());
        let db_path = canonical_root.join("workspace").join(".journal.db");
        let schema_registry_path = canonical_root.join(".journal").join("schemas");
        // Default FileProjection output path = <project_root>/journal.md (root).
        let file_projection_path = canonical_root.join("journal.md");

        let result = JournalInfoResult {
            project_root: canonical_root.clone(),
            db_path: db_path.clone(),
            db_exists: db_path.exists(),
            wal_path: {
                let mut p = db_path.clone().into_os_string();
                p.push("-wal");
                PathBuf::from(p)
            },
            shm_path: {
                let mut p = db_path.clone().into_os_string();
                p.push("-shm");
                PathBuf::from(p)
            },
            schema_registry_path: schema_registry_path.clone(),
            available_schemas: vec!["journal-mcp-canonical-v1".to_string()],
            version: env!("CARGO_PKG_VERSION").to_string(),
            startup_time: "2026-01-01T00:00:00Z".to_string(),
            env_journal_project_root: None,
            file_projection_path: file_projection_path.clone(),
        };

        // All 10 fields must be present (compile-time: struct literal with all fields).
        // Path-typed fields must be absolute.
        assert!(
            result.project_root.is_absolute(),
            "project_root must be absolute, got: {:?}",
            result.project_root
        );
        assert!(
            result.db_path.is_absolute(),
            "db_path must be absolute, got: {:?}",
            result.db_path
        );
        assert!(
            result.wal_path.is_absolute(),
            "wal_path must be absolute, got: {:?}",
            result.wal_path
        );
        assert!(
            result.shm_path.is_absolute(),
            "shm_path must be absolute, got: {:?}",
            result.shm_path
        );
        assert!(
            result.schema_registry_path.is_absolute(),
            "schema_registry_path must be absolute, got: {:?}",
            result.schema_registry_path
        );
        assert!(
            result.file_projection_path.is_absolute(),
            "file_projection_path must be absolute, got: {:?}",
            result.file_projection_path
        );
        // version must be non-empty
        assert!(!result.version.is_empty(), "version must be non-empty");
        // startup_time must be non-empty
        assert!(
            !result.startup_time.is_empty(),
            "startup_time must be non-empty"
        );
    }

    /// Verify that `detect_stale_bak_files` returns entries for all three known
    /// backup prefixes when such files are present in the scanned directory.
    #[test]
    fn test_detect_stale_bak_files_finds_three_prefixes() {
        let tmp = tempfile::TempDir::new().expect("TempDir::new should succeed");
        let dir = tmp.path();

        // Create one file per known prefix.
        let names = [
            ".journal.db.bak.20260615",
            ".journal.db-wal.bak.20260615",
            ".journal.db-shm.bak.20260615",
        ];
        for name in &names {
            std::fs::write(dir.join(name), b"").expect("write bak stub must succeed");
        }

        let found = JournalMcpServer::detect_stale_bak_files(dir);
        assert_eq!(
            found.len(),
            3,
            "expected 3 stale .bak files, got: {:?}",
            found
        );
    }

    // -----------------------------------------------------------------------
    // FileProjection output-path config tests
    // -----------------------------------------------------------------------

    /// T1 (default = root) — after open + close, `<project_root>/journal.md`
    /// is generated and `workspace/journal.md` is NOT generated.
    ///
    /// BREAKING change from v0.2.x where the default was `workspace/journal.md`.
    #[tokio::test]
    async fn test_fp_default_path_is_root() {
        let tmp = tempfile::TempDir::new().expect("TempDir::new");
        // Build server with FileProjection attached (attach=true).
        let server = JournalMcpServer::new_with_config(tmp.path().to_path_buf(), true)
            .expect("new_with_config(true)");

        // Open a chapter, close it — this triggers FileProjection write.
        use rmcp::handler::server::wrapper::Parameters;
        let chapter_id = {
            let params = JournalOpenChapterParams {
                name: "t1-test-chapter".to_string(),
                schema_id: "ytk-canonical-v1".to_string(),
                project_root: None,
            };
            // journal_open_chapter returns Ok(id.0) — a plain UUID string, not JSON.
            server
                .journal_open_chapter(Parameters(params))
                .await
                .expect("open_chapter should succeed")
                .trim()
                .to_string()
        };

        // Append required sections.
        for section in &["Verified", "Done", "Decided", "Not Done", "Issues touched"] {
            let params = JournalAppendSectionParams {
                chapter_id: chapter_id.clone(),
                section_name: section.to_string(),
                body: format!("- {section} content"),
                project_root: None,
            };
            server
                .journal_append_section(Parameters(params))
                .await
                .expect("append_section should succeed");
        }

        // Close the chapter.
        let close_params = JournalCloseChapterParams {
            chapter_id: chapter_id.clone(),
            project_root: None,
        };
        server
            .journal_close_chapter(Parameters(close_params))
            .await
            .expect("close_chapter should succeed");

        // ST7: close_chapter does NOT auto-dispatch FileProjection.
        // We must call journal_projection_rebuild explicitly.
        let rebuild_params = JournalProjectionRebuildParams {
            name: "file".to_string(),
            project_root: None,
            output_path: None,
        };
        server
            .journal_projection_rebuild(Parameters(rebuild_params))
            .await
            .expect("journal_projection_rebuild should succeed");

        // Assert: <project_root>/journal.md (root) must exist after explicit rebuild.
        let root_journal = tmp.path().join("journal.md");
        assert!(
            root_journal.exists(),
            "journal.md must be created at <project_root>/journal.md (root); \
             checked: {root_journal:?}"
        );

        // Assert: workspace/journal.md (old default) must NOT exist.
        let ws_journal = tmp.path().join("workspace").join("journal.md");
        assert!(
            !ws_journal.exists(),
            "workspace/journal.md must NOT be created (v0.3.0 default is root); \
             checked: {ws_journal:?}"
        );
    }

    /// T2 (per-call output_path relative) — `journal_projection_rebuild` with
    /// `output_path=Some("workspace/journal.md")` writes to workspace/journal.md,
    /// while the attached default (root journal.md) is NOT re-touched.
    #[tokio::test]
    async fn test_fp_per_call_relative_output_path() {
        let tmp = tempfile::TempDir::new().expect("TempDir::new");
        let server = JournalMcpServer::new_with_config(tmp.path().to_path_buf(), true)
            .expect("new_with_config(true)");

        use rmcp::handler::server::wrapper::Parameters;

        // Create and close one chapter so the EventLog has something to replay.
        let chapter_id = {
            let params = JournalOpenChapterParams {
                name: "t2-chapter".to_string(),
                schema_id: "ytk-canonical-v1".to_string(),
                project_root: None,
            };
            // journal_open_chapter returns Ok(id.0) — a plain UUID string, not JSON.
            server
                .journal_open_chapter(Parameters(params))
                .await
                .expect("open_chapter")
                .trim()
                .to_string()
        };
        for section in &["Verified", "Done", "Decided", "Not Done", "Issues touched"] {
            let params = JournalAppendSectionParams {
                chapter_id: chapter_id.clone(),
                section_name: section.to_string(),
                body: format!("- {section}"),
                project_root: None,
            };
            server
                .journal_append_section(Parameters(params))
                .await
                .unwrap();
        }
        server
            .journal_close_chapter(Parameters(JournalCloseChapterParams {
                chapter_id,
                project_root: None,
            }))
            .await
            .unwrap();

        // ST7: close_chapter does NOT auto-dispatch FileProjection.
        // Explicitly rebuild to the default (root) path to create journal.md.
        server
            .journal_projection_rebuild(Parameters(JournalProjectionRebuildParams {
                name: "file".to_string(),
                project_root: None,
                output_path: None,
            }))
            .await
            .expect("initial rebuild to root journal.md");

        // Record the last-modified time of root journal.md (written by explicit rebuild).
        let root_journal = tmp.path().join("journal.md");
        assert!(
            root_journal.exists(),
            "root journal.md must exist after explicit rebuild"
        );
        let mtime_before = std::fs::metadata(&root_journal)
            .expect("metadata")
            .modified()
            .ok();

        // Give filesystem a tick so mtime would differ if the file were re-written.
        std::thread::sleep(std::time::Duration::from_millis(50));

        // One-shot rebuild to workspace/journal.md via relative output_path.
        let rebuild_params = JournalProjectionRebuildParams {
            name: "file".to_string(),
            project_root: None,
            output_path: Some("workspace/journal.md".to_string()),
        };
        let res = server
            .journal_projection_rebuild(Parameters(rebuild_params))
            .await
            .expect("rebuild with relative output_path");
        assert!(
            res.contains("one-shot"),
            "rebuild response should mention one-shot; got: {res}"
        );

        // workspace/journal.md must now exist (created by one-shot).
        let ws_journal = tmp.path().join("workspace").join("journal.md");
        assert!(
            ws_journal.exists(),
            "workspace/journal.md must be created by per-call rebuild; checked: {ws_journal:?}"
        );

        // root journal.md must NOT have been re-touched (attached projection unchanged).
        let mtime_after = std::fs::metadata(&root_journal)
            .expect("metadata after")
            .modified()
            .ok();
        assert_eq!(
            mtime_before, mtime_after,
            "root journal.md must not be re-written by one-shot rebuild \
             (attached projection must be unchanged)"
        );
    }

    /// T3 (per-call output_path absolute) — `journal_projection_rebuild` with
    /// an absolute path writes to that path.
    #[tokio::test]
    async fn test_fp_per_call_absolute_output_path() {
        let tmp = tempfile::TempDir::new().expect("TempDir::new for server");
        let tmp_out = tempfile::TempDir::new().expect("TempDir::new for output");
        let server = JournalMcpServer::new_with_config(tmp.path().to_path_buf(), true)
            .expect("new_with_config(true)");

        use rmcp::handler::server::wrapper::Parameters;

        // Create and close one chapter.
        let chapter_id = {
            let params = JournalOpenChapterParams {
                name: "t3-chapter".to_string(),
                schema_id: "ytk-canonical-v1".to_string(),
                project_root: None,
            };
            // journal_open_chapter returns Ok(id.0) — a plain UUID string, not JSON.
            server
                .journal_open_chapter(Parameters(params))
                .await
                .unwrap()
                .trim()
                .to_string()
        };
        for section in &["Verified", "Done", "Decided", "Not Done", "Issues touched"] {
            server
                .journal_append_section(Parameters(JournalAppendSectionParams {
                    chapter_id: chapter_id.clone(),
                    section_name: section.to_string(),
                    body: format!("- {section}"),
                    project_root: None,
                }))
                .await
                .unwrap();
        }
        server
            .journal_close_chapter(Parameters(JournalCloseChapterParams {
                chapter_id,
                project_root: None,
            }))
            .await
            .unwrap();

        // Absolute path target in a different TempDir.
        let abs_output = tmp_out.path().join("elsewhere").join("dump.md");
        let abs_output_str = abs_output
            .to_str()
            .expect("absolute output path must be valid UTF-8")
            .to_string();

        let res = server
            .journal_projection_rebuild(Parameters(JournalProjectionRebuildParams {
                name: "file".to_string(),
                project_root: None,
                output_path: Some(abs_output_str),
            }))
            .await
            .expect("rebuild with absolute output_path");
        assert!(
            res.contains("one-shot"),
            "response must mention one-shot; got: {res}"
        );

        assert!(
            abs_output.exists(),
            "dump.md must be created at absolute path; checked: {abs_output:?}"
        );
    }

    /// T4 (journal_info file_projection_path) — `journal_info()` returns
    /// `file_projection_path` = `<project_root>/journal.md` (absolute).
    #[tokio::test]
    async fn test_fp_journal_info_file_projection_path() {
        let tmp = tempfile::TempDir::new().expect("TempDir::new");
        // Canonicalize to handle macOS /var -> /private/var symlink.
        let canonical_root =
            std::fs::canonicalize(tmp.path()).unwrap_or_else(|_| tmp.path().to_path_buf());

        let server = JournalMcpServer::new_with_config(tmp.path().to_path_buf(), true)
            .expect("new_with_config(true)");

        let info_json = server
            .journal_info()
            .await
            .expect("journal_info should succeed");
        let info: serde_json::Value = serde_json::from_str(&info_json).expect("journal_info JSON");

        let fp_path = info["file_projection_path"]
            .as_str()
            .expect("file_projection_path must be a string");

        // Must be an absolute path ending with journal.md at root.
        let expected = canonical_root.join("journal.md");
        assert_eq!(
            std::path::Path::new(fp_path),
            expected.as_path(),
            "file_projection_path must be <project_root>/journal.md (root); \
             expected: {expected:?}, got: {fp_path}"
        );
    }
}
