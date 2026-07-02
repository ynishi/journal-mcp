//! MCP request DTOs — one `Params` struct per tool, plus [`JournalInfoResult`].
//!
//! Doc comments on each field become MCP wire descriptions (via `schemars`).

use std::path::PathBuf;

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
    ///
    /// Not currently read by the `journal_projection_attach` handler (a
    /// no-op acknowledgement in this release); kept on the wire schema for
    /// parity with the other tool Params and forward compatibility with a
    /// future real re-attach implementation.
    #[serde(default)]
    #[allow(dead_code)]
    pub project_root: Option<String>,
}

/// Parameters for `journal_projection_detach`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct JournalProjectionDetachParams {
    /// Stable name of the projection to detach (e.g. `"file"`).
    ///
    /// Not currently read — `journal_projection_detach` is unsupported in
    /// this release (see `docs/design.md §10 Step 7`) and always returns an
    /// error without inspecting its parameters.
    #[allow(dead_code)]
    pub name: String,
    /// Optional per-call project_root override.  When `None`, the
    /// startup-time default is used.  When `Some(path)`, the server
    /// lazily opens (or reuses a cached) `JournalCore` rooted at the
    /// given path and executes this call against it.
    ///
    /// Not currently read (see `name` field docs above).
    #[serde(default)]
    #[allow(dead_code)]
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
    /// Absolute path to the FileProjection output (current attached default),
    /// or `None` when no `FileProjection` is attached.
    ///
    /// In v0.4.0 the auto-attach default was removed; the projection is only
    /// attached when `JOURNAL_FILE_ENABLE` is set at startup.  When attached,
    /// the path is either `<project_root>/workspace/journal.md` (default) or
    /// the value of `JOURNAL_FILE_OUTPUT_PATH` (resolved as documented in the
    /// crate-level env var notes).
    pub file_projection_path: Option<PathBuf>,
}
