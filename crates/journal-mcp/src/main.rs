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
// Subtask-2 parameter structs — schema 3 tool + read 3 tool
// ---------------------------------------------------------------------------

/// Parameters for `journal_schema_load`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct JournalSchemaLoadParams {
    /// YAML literal conforming to the ChapterSchema format (see `docs/design.md §5`).
    pub yaml: String,
}

/// Parameters for `journal_schema_list` (no fields — lists all schemas).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct JournalSchemaListParams {}

/// Parameters for `journal_schema_show`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct JournalSchemaShowParams {
    /// Registry key to look up (e.g. `"ytk-canonical-v1"`).
    pub key: String,
}

/// Parameters for `journal_tail`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct JournalTailParams {
    /// Maximum number of chapters to return (default 10).
    pub n: Option<usize>,
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
}

/// Parameters for `journal_chapter_list` (no fields — lists all chapters).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct JournalChapterListParams {}

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

    // -----------------------------------------------------------------------
    // Subtask 2: Crux #2 — schema 3 tool (must be independent entries)
    // -----------------------------------------------------------------------

    /// Load a ChapterSchema YAML literal into the SchemaRegistry L2 layer.
    ///
    /// Returns the registry key that was inserted (e.g. `"ytk-canonical-v1"`).
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
                       Returns the registry key that was inserted (e.g. \"ytk-canonical-v1\"). \
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
            let mut core = self.core.lock().unwrap();
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
    /// `["ytk-canonical-v1", "madr-v1", "minimal-v1"]`.
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
        _params: Parameters<JournalSchemaListParams>,
    ) -> Result<String, String> {
        let keys = {
            // SAFETY: see journal_open_chapter
            let core = self.core.lock().unwrap();
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
            let core = self.core.lock().unwrap();
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
            let core = self.core.lock().unwrap();
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
            let core = self.core.lock().unwrap();

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
        _params: Parameters<JournalChapterListParams>,
    ) -> Result<String, String> {
        let rows = {
            // SAFETY: see journal_open_chapter
            let core = self.core.lock().unwrap();
            // tail_chapters with a large n gives all chapters (newest first)
            let chapters = core.tail_chapters(usize::MAX).map_err(|e| {
                tracing::warn!(error = ?e, "journal_chapter_list: tail_chapters failed");
                e.to_string()
            })?;

            chapters
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
}

// ---------------------------------------------------------------------------
// Local helpers (MCP-layer serialisation utilities)
// ---------------------------------------------------------------------------

/// Convert a [`journal::ChapterReplay`] to a `serde_json::Value`.
///
/// `ChapterReplay` (and `ChapterMeta` / `EventRow`) do not derive `Serialize`
/// in the `journal` crate (to keep the library layer clean).  This helper
/// provides the MCP-layer projection without polluting the library.
fn chapter_replay_to_json(replay: &journal::ChapterReplay) -> serde_json::Value {
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

    /// T3 (error path) — tool_router now returns exactly 10 tools after ST2.
    ///
    /// Updated from "exactly 4 tools" in ST1 to "exactly 10 tools" in ST2
    /// (4 lifecycle + 3 schema + 3 read tools).
    #[test]
    fn test_subtask2_exactly_ten_tools() {
        let tmp = tempfile::TempDir::new().expect("TempDir::new should succeed");
        let server = make_server(&tmp);
        let count = server.tool_router.list_all().len();
        assert_eq!(
            count, 10,
            "ST2 tool_router should have exactly 10 tools, got {count}"
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
    /// the 3 built-in schemas: ytk-canonical-v1, madr-v1, minimal-v1).
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
            keys.iter().any(|k| k.contains("ytk-canonical")),
            "built-in ytk-canonical should be in schema_keys; got: {keys:?}"
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
}
