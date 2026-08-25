//! The 18 `#[tool]`-annotated MCP tool handlers for [`JournalMcpServer`],
//! registered in a single `#[tool_router]` block (Crux #1: tool_router 一元
//! ServerHandler 配線).

use std::path::PathBuf;

use rmcp::{handler::server::wrapper::Parameters, tool, tool_router};

use crate::request::{
    JournalAppendProgressParams, JournalAppendSectionParams, JournalChapterListParams,
    JournalCloseChapterParams, JournalDumpParams, JournalGrepParams, JournalImportParams,
    JournalInfoResult, JournalOpenChapterParams, JournalOpenChaptersParams, JournalProgressOfParams,
    JournalProjectionAttachParams, JournalProjectionDetachParams, JournalProjectionRebuildParams,
    JournalSchemaListParams, JournalSchemaLoadParams, JournalSchemaShowParams, JournalTailParams,
};
use crate::server::JournalMcpServer;

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
// Tool implementations — Crux #1: all tools in a single #[tool_router] block
// ---------------------------------------------------------------------------

/// All journal-mcp MCP tools are registered in this single block.
///
/// This satisfies Crux #1 (tool_router 一元 ServerHandler 配線): all 18
/// tools live here, and nowhere else.
#[tool_router(vis = "pub(crate)")]
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

    /// Render the entire journal to a single Markdown string.
    ///
    /// Render-to-string counterpart of the `FileProjection`: the rendered
    /// `journal.md`-equivalent content is returned as the tool result and
    /// **no file is written on the server**.  The caller (a local MCP host,
    /// an AI session, or a forwarding layer like lds) decides where to
    /// materialize it — this is what lets a remote journal daemon hand a
    /// local file back to the client machine.
    #[tool(
        name = "journal_dump",
        description = "Render the entire journal to a single Markdown string (journal.md \
                       equivalent) and return it as the tool result — no file is written \
                       on the server. Chapters are ordered oldest-first by chapter id. \
                       Optional since (Unix epoch ms) filters chapters by opened_at. \
                       Use this to materialize a local journal.md from a remote daemon.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn journal_dump(
        &self,
        Parameters(params): Parameters<JournalDumpParams>,
    ) -> Result<String, String> {
        let markdown = {
            // SAFETY: see journal_open_chapter
            let core_handle = self
                .resolve_core(params.project_root.as_deref())
                .map_err(|e| format!("resolve_core: {e}"))?;
            let core = core_handle.lock().unwrap();
            core.dump_markdown(params.since).map_err(|e| {
                tracing::warn!(error = ?e, "journal_dump failed");
                e.to_string()
            })?
        }; // guard drops here
        Ok(markdown)
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
            let paginated = JournalMcpServer::paginate(chapters, params.offset, params.limit);

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

    /// Attach a named projection.
    ///
    /// As of v0.4.0 the startup auto-attach for `"file"` was removed; the
    /// projection is now attached only when `JOURNAL_FILE_ENABLE` is set at
    /// startup.  Runtime attach via this tool is currently a no-op
    /// acknowledgement — to actually re-route file output at runtime, restart
    /// the server with the appropriate env vars or use the per-call
    /// `output_path` argument of `journal_projection_rebuild`.  Full runtime
    /// re-attach support is tracked separately (carry from v0.4.0).
    /// Requesting any name other than `"file"` returns an error.
    #[tool(
        name = "journal_projection_attach",
        description = "Attach a named projection. Currently only 'file' is recognised. \
                       Runtime attach is acknowledged but does not re-route output; \
                       set JOURNAL_FILE_ENABLE at server startup, or use the \
                       per-call output_path argument of journal_projection_rebuild \
                       for one-shot writes. Other names return an error.",
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
                    "journal_projection_attach: 'file' acknowledged \
                     (runtime re-attach is a no-op; set JOURNAL_FILE_ENABLE at startup \
                     or use journal_projection_rebuild's output_path argument)"
                );
                Ok(
                    "file projection acknowledged (set JOURNAL_FILE_ENABLE at startup \
                    or use journal_projection_rebuild's output_path argument)"
                        .to_string(),
                )
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
// Tests — moved verbatim from the pre-split journal-mcp/src/main.rs.
//
// This module lives here (rather than in server.rs) because most tests call
// the private `#[tool]` handler methods above directly (e.g.
// `server.journal_open_chapter(...)`), which are only visible to `tools`
// and its descendant modules.  Tests for `resolve_file_projection_path`
// (env-var resolution, a `journal-mcp` binary-crate concern since the
// rmcp/server split) live in the `journal-mcp` bin crate instead.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    /// Build a `JournalMcpServer` backed by a temporary directory.
    ///
    /// FileProjection is not attached (force-disabled) so that tests do not
    /// touch the real filesystem outside the `TempDir` (Crux #3) and do not
    /// race against ambient `JOURNAL_FILE_ENABLE` / `JOURNAL_FILE_OUTPUT_PATH`
    /// env state.
    fn make_server(tmp: &tempfile::TempDir) -> JournalMcpServer {
        JournalMcpServer::new_without_file_attach(tmp.path().to_path_buf())
            // SAFETY: TempDir is kept alive by caller; the ctor creates the workspace/ subdir.
            .expect("JournalMcpServer::new_without_file_attach should succeed in temp dir")
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

    /// T3 (error path) — tool_router now returns exactly 18 tools.
    ///
    /// Updated from "exactly 17 tools" (journal_info) to "exactly 18 tools"
    /// by adding `journal_dump` (render-to-string, remote-mode primitive).
    ///
    /// Verifies Crux #1: all 18 tools are wired into the single `#[tool_router]` block.
    #[test]
    fn test_st7_exactly_seventeen_tools() {
        let tmp = tempfile::TempDir::new().expect("TempDir::new should succeed");
        let server = make_server(&tmp);
        let count = server.tool_router.list_all().len();
        assert_eq!(
            count, 18,
            "tool_router should have exactly 18 tools (Crux #1), got {count}"
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

    /// Canonical spelling of all 18 MCP tools in the tool_router.
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
        // remote-mode: render-to-string dump (1)
        "journal_dump",
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

    /// T1 (property) — Crux #1 final: all 18 tools are registered in the
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
            18,
            "exactly 18 tools must be registered (Crux #1); got: {tool_names:?}"
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
    // Test-only constructor isolation tests — TempDir-only path guarantee
    // -----------------------------------------------------------------------

    /// T1 (property) — `new_without_file_attach` succeeds and returns a
    /// working server with no FileProjection attached.
    ///
    /// Verifies Crux #3: test-only constructor must succeed and all file paths
    /// must remain inside the TempDir (no real journal.md touched).
    #[test]
    fn test_new_without_file_attach_succeeds() {
        let tmp = tempfile::TempDir::new().expect("TempDir::new should succeed");
        let result = JournalMcpServer::new_without_file_attach(tmp.path().to_path_buf());
        assert!(
            result.is_ok(),
            "new_without_file_attach should return Ok; got: {:?}",
            result.err()
        );
        // The workspace directory must have been created inside TempDir.
        let workspace = tmp.path().join("workspace");
        assert!(
            workspace.exists(),
            "workspace dir should be created inside TempDir by ctor"
        );
        // No projection is attached, so no journal.md file should exist anywhere.
        let root_journal_md = tmp.path().join("journal.md");
        assert!(
            !root_journal_md.exists(),
            "journal.md must not be created when no projection attached; \
             path: {root_journal_md:?}"
        );
        let workspace_journal_md = tmp.path().join("workspace").join("journal.md");
        assert!(
            !workspace_journal_md.exists(),
            "workspace/journal.md must not be created; path: {workspace_journal_md:?}"
        );
        // file_projection_path on the server struct must reflect that no
        // projection was attached.
        let server = result.expect("just asserted Ok");
        assert!(
            server.file_projection_path.is_none(),
            "file_projection_path must be None when no projection attached; \
            got: {:?}",
            server.file_projection_path
        );
    }

    /// T2 (boundary) — `new_with_file_attach(_, path)` attaches a
    /// FileProjection at the given path; server constructs successfully and
    /// file_projection_path reflects the resolved absolute path.
    #[test]
    fn test_new_with_file_attach_constructs_ok() {
        let tmp = tempfile::TempDir::new().expect("TempDir::new should succeed");
        let target = tmp.path().join("workspace").join("journal.md");
        let result =
            JournalMcpServer::new_with_file_attach(tmp.path().to_path_buf(), target.clone());
        assert!(
            result.is_ok(),
            "new_with_file_attach should return Ok; got: {:?}",
            result.err()
        );
        let server = result.expect("just asserted Ok");
        assert_eq!(
            server.file_projection_path.as_deref(),
            Some(target.as_path()),
            "file_projection_path must equal the force-attached path"
        );
    }

    /// T3 (relative path) — `new_with_file_attach` with a relative path
    /// resolves against `project_root`.
    #[test]
    fn test_new_with_file_attach_relative_path_resolves_to_project_root() {
        let tmp = tempfile::TempDir::new().expect("TempDir::new should succeed");
        let server = JournalMcpServer::new_with_file_attach(
            tmp.path().to_path_buf(),
            PathBuf::from("workspace/journal.md"),
        )
        .expect("ctor should succeed");
        let expected = tmp.path().join("workspace").join("journal.md");
        assert_eq!(
            server.file_projection_path.as_deref(),
            Some(expected.as_path()),
            "relative path must resolve against project_root"
        );
    }

    /// T4 (error path) — `new_without_file_attach` propagates an error when
    /// the project root cannot have its workspace subdir created (invalid
    /// nested path).
    #[test]
    fn test_new_without_file_attach_returns_err_on_bad_root() {
        let tmp = tempfile::TempDir::new().expect("TempDir::new should succeed");
        // Create a regular file where `workspace` would need to be a directory.
        let blocker = tmp.path().join("workspace");
        std::fs::write(&blocker, b"blocking file").expect("write should succeed");
        let nested_root = blocker.join("subpath");
        let result = JournalMcpServer::new_without_file_attach(nested_root);
        assert!(
            result.is_err(),
            "ctor should return Err when workspace dir cannot be created"
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
        // v0.4.0 default FileProjection output path when env-enabled
        // (verifying both the Some(absolute) shape and the new default location).
        let file_projection_path = canonical_root.join("workspace").join("journal.md");

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
            file_projection_path: Some(file_projection_path.clone()),
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
        // file_projection_path is Option<PathBuf> in v0.4.0.
        let fp = result
            .file_projection_path
            .as_deref()
            .expect("Some(path) provided in this test case");
        assert!(
            fp.is_absolute(),
            "file_projection_path (when Some) must be absolute, got: {fp:?}"
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
    // FileProjection per-call output_path tests (one-shot rebuild)
    // -----------------------------------------------------------------------

    /// T1 (per-call output_path relative) — `journal_projection_rebuild` with
    /// `output_path=Some("workspace/journal.md")` writes to workspace/journal.md.
    /// Uses force-attached projection at a different path; the attached projection
    /// must NOT be re-touched by the one-shot.
    #[tokio::test]
    async fn test_fp_per_call_relative_output_path() {
        let tmp = tempfile::TempDir::new().expect("TempDir::new");
        let attached = tmp.path().join("custom").join("attached.md");
        let server =
            JournalMcpServer::new_with_file_attach(tmp.path().to_path_buf(), attached.clone())
                .expect("new_with_file_attach");

        use rmcp::handler::server::wrapper::Parameters;

        // Create and close one chapter so the EventLog has something to replay.
        let chapter_id = {
            let params = JournalOpenChapterParams {
                name: "t2-chapter".to_string(),
                schema_id: "ytk-canonical-v1".to_string(),
                project_root: None,
            };
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

        // Explicit rebuild to the force-attached (custom) path to seed it.
        server
            .journal_projection_rebuild(Parameters(JournalProjectionRebuildParams {
                name: "file".to_string(),
                project_root: None,
                output_path: None,
            }))
            .await
            .expect("initial rebuild to attached path");

        assert!(
            attached.exists(),
            "attached projection file must exist after explicit rebuild"
        );
        let mtime_before = std::fs::metadata(&attached)
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

        // Attached projection file must NOT have been re-touched.
        let mtime_after = std::fs::metadata(&attached)
            .expect("metadata after")
            .modified()
            .ok();
        assert_eq!(
            mtime_before, mtime_after,
            "attached projection file must not be re-written by one-shot rebuild \
             (attached projection must be unchanged)"
        );
    }

    /// T2 (per-call output_path absolute) — `journal_projection_rebuild` with
    /// an absolute path writes to that path.
    #[tokio::test]
    async fn test_fp_per_call_absolute_output_path() {
        let tmp = tempfile::TempDir::new().expect("TempDir::new for server");
        let tmp_out = tempfile::TempDir::new().expect("TempDir::new for output");
        // Use no-attach: per-call rebuild does not require an attached projection.
        let server = JournalMcpServer::new_without_file_attach(tmp.path().to_path_buf())
            .expect("new_without_file_attach");

        use rmcp::handler::server::wrapper::Parameters;

        // Create and close one chapter.
        let chapter_id = {
            let params = JournalOpenChapterParams {
                name: "t3-chapter".to_string(),
                schema_id: "ytk-canonical-v1".to_string(),
                project_root: None,
            };
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

    /// T3 (journal_info file_projection_path = Some) — when force-attached,
    /// `journal_info()` returns `file_projection_path` as the resolved
    /// absolute path string.
    #[tokio::test]
    async fn test_fp_journal_info_file_projection_path_attached() {
        let tmp = tempfile::TempDir::new().expect("TempDir::new");
        // Canonicalize to handle macOS /var -> /private/var symlink.
        let canonical_root =
            std::fs::canonicalize(tmp.path()).unwrap_or_else(|_| tmp.path().to_path_buf());
        let target = canonical_root.join("workspace").join("journal.md");

        let server =
            JournalMcpServer::new_with_file_attach(tmp.path().to_path_buf(), target.clone())
                .expect("new_with_file_attach");

        let info_json = server
            .journal_info()
            .await
            .expect("journal_info should succeed");
        let info: serde_json::Value = serde_json::from_str(&info_json).expect("journal_info JSON");

        let fp_path = info["file_projection_path"]
            .as_str()
            .expect("file_projection_path must be a string when attached");

        assert_eq!(
            std::path::Path::new(fp_path),
            target.as_path(),
            "file_projection_path must equal the force-attached path; \
             expected: {target:?}, got: {fp_path}"
        );
    }

    /// T4 (journal_info file_projection_path = null) — when no projection is
    /// attached, `journal_info()` returns `file_projection_path` as JSON null.
    #[tokio::test]
    async fn test_fp_journal_info_file_projection_path_none_when_no_attach() {
        let tmp = tempfile::TempDir::new().expect("TempDir::new");
        let server = JournalMcpServer::new_without_file_attach(tmp.path().to_path_buf())
            .expect("new_without_file_attach");

        let info_json = server
            .journal_info()
            .await
            .expect("journal_info should succeed");
        let info: serde_json::Value = serde_json::from_str(&info_json).expect("journal_info JSON");

        assert!(
            info["file_projection_path"].is_null(),
            "file_projection_path must be null when no projection is attached; \
             got: {}",
            info["file_projection_path"]
        );
    }

    /// T5 (runtime journal_projection_attach is a no-op acknowledgement in
    /// v0.4.0) — calling `journal_projection_attach(name="file")` when no
    /// projection is attached succeeds with an acknowledgement message but
    /// does NOT cause a file to be created.
    #[tokio::test]
    async fn test_fp_runtime_attach_is_acknowledgement_only() {
        let tmp = tempfile::TempDir::new().expect("TempDir::new");
        let server = JournalMcpServer::new_without_file_attach(tmp.path().to_path_buf())
            .expect("new_without_file_attach");

        use rmcp::handler::server::wrapper::Parameters;

        let res = server
            .journal_projection_attach(Parameters(JournalProjectionAttachParams {
                name: "file".to_string(),
                project_root: None,
            }))
            .await
            .expect("attach 'file' should return Ok (acknowledgement)");
        assert!(
            res.contains("JOURNAL_FILE_ENABLE") || res.contains("journal_projection_rebuild"),
            "acknowledgement should hint at the supported re-attach paths; got: {res}"
        );

        // No file should be created by the acknowledgement alone.
        let ws_journal = tmp.path().join("workspace").join("journal.md");
        let root_journal = tmp.path().join("journal.md");
        assert!(
            !ws_journal.exists(),
            "workspace/journal.md must not be created by runtime attach"
        );
        assert!(
            !root_journal.exists(),
            "<root>/journal.md must not be created by runtime attach"
        );
    }
}
