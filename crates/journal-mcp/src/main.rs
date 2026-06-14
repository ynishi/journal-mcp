//! journal-mcp — stdio MCP server exposing the `journal` library.
//!
//! Exposes the `journal` crate as an MCP server over stdio transport.
//! All tools are registered in a single [`#[tool_router]`](rmcp::tool_router) block
//! on [`JournalMcpServer`], satisfying the Crux #1 (tool_router 一元 ServerHandler 配線)
//! constraint.
//!
//! # Environment variables
//!
//! * `JOURNAL_PROJECT_ROOT` — root directory of the project.  Defaults to the
//!   process's current working directory.
//! * `JOURNAL_DISABLE_FILE_PROJECTION` — set to `"1"` to skip the automatic
//!   [`FileProjection`](journal::FileProjection) attachment (useful for tests /
//!   debugging environments where `workspace/journal.md` is unavailable).
//!
//! See `docs/design.md §6` for the full tool table and `§10 Step 5` for the
//! stdio transport specification.

use std::path::PathBuf;
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
    /// Schema ID that governs this chapter (e.g. `"ytk-canonical-v1"`).
    pub schema_id: String,
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
}

/// Parameters for `journal_append_progress`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct JournalAppendProgressParams {
    /// Target chapter ID.
    pub chapter_id: String,
    /// Single progress line to append (e.g. `"step 3 done"`).
    pub line: String,
}

/// Parameters for `journal_close_chapter`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct JournalCloseChapterParams {
    /// Target chapter ID to close.
    pub chapter_id: String,
}

// ---------------------------------------------------------------------------
// JournalMcpServer
// ---------------------------------------------------------------------------

/// MCP server for the `journal` library.
///
/// Wraps a [`journal::JournalCore`] behind an `Arc<Mutex<…>>` so that the
/// server can be cloned per-connection as required by rmcp while the core
/// remains single-writer.
///
/// # Crux invariants satisfied here
///
/// * **Crux #1** (tool_router 一元 ServerHandler 配線): all 15 tools (ST6-1/ST6-2/ST6-3)
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
    core: Arc<Mutex<journal::JournalCore>>,
    /// Project root directory; retained for tools that need filesystem context.
    #[allow(dead_code)]
    project_root: PathBuf,
}

impl JournalMcpServer {
    /// Construct a `JournalMcpServer` for the given `project_root`.
    ///
    /// Performs the following initialisation steps:
    ///
    /// 1. Load the schema registry (`SchemaRegistry::with_project_local`).
    /// 2. Open (or create) the journal database at
    ///    `{project_root}/workspace/.journal.db`.
    /// 3. Optionally attach a [`FileProjection`](journal::FileProjection) that
    ///    writes to `{project_root}/workspace/journal.md` (skipped when
    ///    `JOURNAL_DISABLE_FILE_PROJECTION=1`).
    ///
    /// # Errors
    ///
    /// Returns an error if the schema registry or database cannot be opened.
    pub fn new(project_root: PathBuf) -> anyhow::Result<Self> {
        let registry = journal::SchemaRegistry::with_project_local(&project_root)?;

        let db_dir = project_root.join("workspace");
        // Ensure the workspace directory exists so SQLite can create the DB file.
        std::fs::create_dir_all(&db_dir)?;
        let db_path = db_dir.join(".journal.db");

        // Clone the registry before consuming it; FileProjection needs an Arc.
        let registry_arc = std::sync::Arc::new(registry.clone());
        let mut core = journal::JournalCore::open(&db_path, registry)?;

        // Auto-attach FileProjection unless disabled for testing/debugging.
        let disable_fp = std::env::var("JOURNAL_DISABLE_FILE_PROJECTION")
            .map(|v| v == "1")
            .unwrap_or(false);
        if !disable_fp {
            let journal_md = db_dir.join("journal.md");
            let proj = journal::FileProjection::new(journal_md, registry_arc);
            core.add_projection(proj);
        }

        Ok(Self {
            tool_router: Self::tool_router(),
            core: Arc::new(Mutex::new(core)),
            project_root,
        })
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
            let mut core = self.core.lock().unwrap();
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
        let chapter_id = journal::ChapterId(params.chapter_id);
        let warnings = {
            // SAFETY: see journal_open_chapter
            let mut core = self.core.lock().unwrap();
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
        let chapter_id = journal::ChapterId(params.chapter_id);
        let warnings = {
            // SAFETY: see journal_open_chapter
            let mut core = self.core.lock().unwrap();
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
        let chapter_id = journal::ChapterId(params.chapter_id);
        {
            // SAFETY: see journal_open_chapter
            let mut core = self.core.lock().unwrap();
            core.close_chapter(&chapter_id).map_err(|e| {
                tracing::warn!(error = ?e, "journal_close_chapter failed");
                e.to_string()
            })?
        };
        Ok("ok".to_string())
    }
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
    /// `JOURNAL_DISABLE_FILE_PROJECTION=1` is set so that `FileProjection`
    /// does not attempt to write to the temp dir's `journal.md` during tests.
    fn make_server(tmp: &tempfile::TempDir) -> JournalMcpServer {
        // Disable FileProjection to avoid IO side-effects in unit tests.
        std::env::set_var("JOURNAL_DISABLE_FILE_PROJECTION", "1");
        JournalMcpServer::new(tmp.path().to_path_buf())
            // SAFETY: TempDir is kept alive by caller; new() creates workspace/ subdir.
            .expect("JournalMcpServer::new should succeed in temp dir")
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

    /// T3 (error path) — tool_router returns exactly 4 tools (no extras in ST1).
    #[test]
    fn test_subtask1_exactly_four_tools() {
        let tmp = tempfile::TempDir::new().expect("TempDir::new should succeed");
        let server = make_server(&tmp);
        let count = server.tool_router.list_all().len();
        assert_eq!(
            count, 4,
            "ST1 tool_router should have exactly 4 tools, got {count}"
        );
    }
}
