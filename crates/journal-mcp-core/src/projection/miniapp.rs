//! `MiniAppProjection` — sync chapter metadata to a mini-app table.
//!
//! Projects each chapter as a row in a mini-app `journal_chapter` table
//! (or a caller-supplied table name) so that downstream consumers can
//! SQL-filter chapters and join them with other mini-app tables
//! (e.g. chapter ↔ issue ↔ commit cross-references).
//!
//! # Design split (δ-1 / δ-2)
//!
//! This module implements **δ-1**: the projection logic + the
//! [`MiniAppClient`] trait abstraction over the mini-app-mcp wire surface.
//! All chapter-write operations route through the trait, which is
//! mock-implementable for testing.
//!
//! The concrete `rmcp` child-process client (`RmcpStdioMiniAppClient`)
//! that spawns the real `mini-app-mcp` binary and talks to it over stdio
//! is **δ-2** and lands in a follow-up commit on the same topic branch
//! alongside the sibling γ-2 (Outline rmcp client) commit.  Both clients
//! share the same `rmcp` child-process wrapper pattern.
//!
//! # Row mapping
//!
//! Each chapter projects to a single row in the configured mini-app table.
//! The row's `data` payload has the following shape:
//!
//! ```json
//! {
//!   "chapter_id": "2026-06-15-...",
//!   "project_label": "myproject",
//!   "schema_id": "journal-mcp-canonical-v1",
//!   "current_state": "closed",
//!   "opened_at": 1781502216733,
//!   "closed_at": 1781502370716,
//!   "decided_summary": "...",
//!   "issue_refs": ["a7cea6d7-...", "98123835-..."]
//! }
//! ```
//!
//! `decided_summary` is the first line of the `Decided` section body
//! (or empty string if the section is missing).  `issue_refs` is the
//! list of mini-app UUIDs extracted from the `Issues touched` section
//! body via regex match.

use std::collections::HashSet;

use serde::Serialize;

use super::{private::Sealed, JournalProjection, ProjectionError};
use crate::{ChapterId, ChapterReplay};

// ---------------------------------------------------------------------------
// MiniAppClient trait — wire-format abstraction
// ---------------------------------------------------------------------------

/// Minimal wire surface that [`MiniAppProjection`] requires from a
/// mini-app-mcp client.
///
/// Implementations route to either the real `rmcp` child-process
/// mini-app-mcp server (δ-2) or a test mock.
///
/// The trait surface follows the mini-app-mcp tool taxonomy:
/// - `schema_ensure`: idempotent table creation (no-op if the table
///   exists with the expected schema).
/// - `row_query_by_chapter_id`: look up a row by its `chapter_id` field
///   value.  Returns `Ok(Some(row_id))` if found, `Ok(None)` if absent.
/// - `row_create`: insert a new row with the given JSON data.
/// - `row_update`: update an existing row's JSON data (merge semantics
///   per mini-app-mcp's default update mode).
pub trait MiniAppClient: Send {
    /// Ensure the table exists; auto-deploy a schema if absent.
    ///
    /// `schema_yaml` is the YAML schema definition to deploy if the
    /// table is not yet present in the mini-app store.  Idempotent: if
    /// the table already exists, this is a no-op.
    fn schema_ensure(&mut self, table: &str, schema_yaml: &str) -> Result<(), ProjectionError>;

    /// Query for a row by its `chapter_id` field value.
    ///
    /// Returns `Ok(Some(row_id))` when a row matches, `Ok(None)` when
    /// no row has the given `chapter_id`, or `Err(...)` on transport
    /// failure.
    fn row_query_by_chapter_id(
        &mut self,
        table: &str,
        chapter_id: &str,
    ) -> Result<Option<String>, ProjectionError>;

    /// Insert a new row with the supplied JSON data.
    ///
    /// Returns the newly-created `row_id` on success.
    fn row_create(
        &mut self,
        table: &str,
        data: &serde_json::Value,
    ) -> Result<String, ProjectionError>;

    /// Update an existing row's JSON data.
    fn row_update(
        &mut self,
        table: &str,
        row_id: &str,
        data: &serde_json::Value,
    ) -> Result<(), ProjectionError>;
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for [`MiniAppProjection`].
#[derive(Debug, Clone)]
pub struct MiniAppConfig {
    /// Target mini-app table name (e.g. `"journal_chapter"`).
    pub table_name: String,
    /// Label that populates the `project_label` field on each row.
    /// Lets downstream queries filter rows by source project when one
    /// mini-app store aggregates chapters from multiple projects.
    pub project_label: String,
}

impl Default for MiniAppConfig {
    fn default() -> Self {
        Self {
            table_name: "journal_chapter".to_owned(),
            project_label: "journal".to_owned(),
        }
    }
}

/// Schema YAML embedded into the binary; auto-deployed on first sync
/// when the target table is absent.
const SCHEMA_YAML: &str = include_str!("miniapp_schema.yaml");

// ---------------------------------------------------------------------------
// Row payload shape
// ---------------------------------------------------------------------------

/// JSON shape of a single row's `data` payload.
///
/// Field order matches the schema YAML for readable diffs when the row
/// is inspected via mini-app-mcp's CLI.
#[derive(Debug, Serialize)]
struct RowPayload<'a> {
    chapter_id: &'a str,
    project_label: &'a str,
    schema_id: &'a str,
    current_state: &'a str,
    opened_at: i64,
    closed_at: Option<i64>,
    decided_summary: String,
    issue_refs: Vec<String>,
}

// ---------------------------------------------------------------------------
// MiniAppProjection
// ---------------------------------------------------------------------------

/// Mini-app sync projection, generic over the [`MiniAppClient`]
/// implementation.
pub struct MiniAppProjection<C: MiniAppClient> {
    /// Client that talks to the mini-app-mcp backend.
    client: C,

    /// Static configuration (table name + project label).
    config: MiniAppConfig,

    /// Chapter IDs queued for rebuild on the next batch flush.
    dirty: HashSet<String>,

    /// Whether `schema_ensure` has already been called for this table
    /// during the lifetime of this `MiniAppProjection`.  Avoids repeated
    /// wire calls in the common steady-state path.
    schema_initialized: bool,
}

impl<C: MiniAppClient> MiniAppProjection<C> {
    /// Construct a `MiniAppProjection` with the given client and config.
    ///
    /// The `schema_ensure` wire call is deferred to the first
    /// `rebuild_chapter` invocation; `new` itself does not touch the
    /// client.
    pub fn new(client: C, config: MiniAppConfig) -> Self {
        Self {
            client,
            config,
            dirty: HashSet::new(),
            schema_initialized: false,
        }
    }

    /// Return a reference to the set of dirty chapter IDs.
    ///
    /// Used by tests to inspect internal state.
    pub fn dirty_chapters(&self) -> &HashSet<String> {
        &self.dirty
    }

    /// Extract the first non-empty line of the `Decided` section body
    /// from the chapter replay, returning an empty string if the section
    /// is absent or has empty body.
    fn extract_decided_summary(replay: &ChapterReplay) -> Result<String, ProjectionError> {
        for event in &replay.events {
            if event.event_type != "section_append" {
                continue;
            }
            if event.section_name.as_deref() != Some("Decided") {
                continue;
            }
            let payload: serde_json::Value = serde_json::from_str(&event.payload)?;
            let body = payload
                .get("body")
                .and_then(|b| b.as_str())
                .unwrap_or_default();
            if let Some(first_line) = body.lines().find(|l| !l.trim().is_empty()) {
                return Ok(first_line.trim().to_owned());
            }
            return Ok(String::new());
        }
        Ok(String::new())
    }

    /// Extract mini-app UUIDs (8-4-4-4-12 hex pattern) from the
    /// `Issues touched` section body.  Returns an empty vector if the
    /// section is absent.
    ///
    /// Matches only canonical UUID v4-shaped strings (`[a-f0-9]{8}-[a-f0-9]{4}-…`).
    /// Case-insensitive on the hex digits.
    fn extract_issue_refs(replay: &ChapterReplay) -> Result<Vec<String>, ProjectionError> {
        for event in &replay.events {
            if event.event_type != "section_append" {
                continue;
            }
            if event.section_name.as_deref() != Some("Issues touched") {
                continue;
            }
            let payload: serde_json::Value = serde_json::from_str(&event.payload)?;
            let body = payload
                .get("body")
                .and_then(|b| b.as_str())
                .unwrap_or_default();
            return Ok(scan_uuids(body));
        }
        Ok(Vec::new())
    }

    /// Build the JSON `data` payload for the row corresponding to `replay`.
    fn build_payload(&self, replay: &ChapterReplay) -> Result<serde_json::Value, ProjectionError> {
        let payload = RowPayload {
            chapter_id: &replay.meta.chapter_id.0,
            project_label: &self.config.project_label,
            schema_id: &replay.meta.schema_id,
            current_state: &replay.meta.current_state,
            opened_at: replay.meta.opened_at,
            closed_at: replay.meta.closed_at,
            decided_summary: Self::extract_decided_summary(replay)?,
            issue_refs: Self::extract_issue_refs(replay)?,
        };
        Ok(serde_json::to_value(&payload)?)
    }
}

impl<C: MiniAppClient> Sealed for MiniAppProjection<C> {}

impl<C: MiniAppClient + 'static> JournalProjection for MiniAppProjection<C> {
    fn name(&self) -> &'static str {
        "miniapp"
    }

    fn mark_dirty(&mut self, id: &ChapterId) -> Result<(), ProjectionError> {
        self.dirty.insert(id.0.clone());
        Ok(())
    }

    /// Sync the chapter to its corresponding mini-app row.
    ///
    /// Pipeline:
    /// 1. Lazy `schema_ensure` on first call (idempotent for subsequent
    ///    calls within the same process).
    /// 2. Build the row payload (chapter metadata + decided_summary +
    ///    issue_refs).
    /// 3. Query mini-app for an existing row by `chapter_id`.
    /// 4. If found, call `row_update` with the new payload; otherwise
    ///    call `row_create` to insert.
    /// 5. Clear the dirty entry for this chapter on success.
    fn rebuild_chapter(&mut self, replay: &ChapterReplay) -> Result<(), ProjectionError> {
        if !self.schema_initialized {
            self.client
                .schema_ensure(&self.config.table_name, SCHEMA_YAML)?;
            self.schema_initialized = true;
        }

        let chapter_id = replay.meta.chapter_id.0.clone();
        let payload = self.build_payload(replay)?;

        let existing = self
            .client
            .row_query_by_chapter_id(&self.config.table_name, &chapter_id)?;

        match existing {
            Some(row_id) => {
                self.client
                    .row_update(&self.config.table_name, &row_id, &payload)?;
            }
            None => {
                self.client.row_create(&self.config.table_name, &payload)?;
            }
        }

        self.dirty.remove(&chapter_id);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// UUID extraction helper
// ---------------------------------------------------------------------------

/// Scan `text` for canonical UUID v4-shaped substrings and return them
/// in match order.
///
/// Implemented as a manual sliding-window scanner so that the
/// `journal-mcp-core` crate does not pick up a `regex` dependency for
/// this single use site (the rest of the crate is regex-free).
fn scan_uuids(text: &str) -> Vec<String> {
    fn is_hex(c: char) -> bool {
        c.is_ascii_hexdigit()
    }
    fn count_hex(window: &[char]) -> usize {
        window.iter().take_while(|c| is_hex(**c)).count()
    }

    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 36 <= chars.len() {
        let w = &chars[i..i + 36];
        // 8-4-4-4-12 pattern: positions 8, 13, 18, 23 must be '-'; the
        // other positions must be hex digits.
        if w[8] == '-'
            && w[13] == '-'
            && w[18] == '-'
            && w[23] == '-'
            && count_hex(&w[0..8]) == 8
            && count_hex(&w[9..13]) == 4
            && count_hex(&w[14..18]) == 4
            && count_hex(&w[19..23]) == 4
            && count_hex(&w[24..36]) == 12
        {
            let uuid: String = w.iter().collect();
            // Skip if immediately preceded or followed by another hex
            // digit (so we don't match the middle of a longer hex run).
            let left_clean = i == 0 || !is_hex(chars[i - 1]);
            let right_clean = i + 36 == chars.len() || !is_hex(chars[i + 36]);
            if left_clean && right_clean {
                out.push(uuid.to_ascii_lowercase());
                i += 36;
                continue;
            }
        }
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_log::{ChapterMeta, EventId, EventRow};

    /// Mock client that records all wire calls for assertion.
    #[derive(Default)]
    struct MockMiniAppClient {
        existing: std::collections::HashMap<(String, String), String>,
        schema_calls: Vec<(String, String)>,
        queried: Vec<(String, String)>,
        created: Vec<(String, serde_json::Value)>,
        updated: Vec<(String, String, serde_json::Value)>,
    }

    impl MockMiniAppClient {
        fn with_existing(mut self, table: &str, chapter_id: &str, row_id: &str) -> Self {
            self.existing
                .insert((table.to_owned(), chapter_id.to_owned()), row_id.to_owned());
            self
        }
    }

    impl MiniAppClient for MockMiniAppClient {
        fn schema_ensure(&mut self, table: &str, schema_yaml: &str) -> Result<(), ProjectionError> {
            self.schema_calls
                .push((table.to_owned(), schema_yaml.to_owned()));
            Ok(())
        }

        fn row_query_by_chapter_id(
            &mut self,
            table: &str,
            chapter_id: &str,
        ) -> Result<Option<String>, ProjectionError> {
            self.queried.push((table.to_owned(), chapter_id.to_owned()));
            Ok(self
                .existing
                .get(&(table.to_owned(), chapter_id.to_owned()))
                .cloned())
        }

        fn row_create(
            &mut self,
            table: &str,
            data: &serde_json::Value,
        ) -> Result<String, ProjectionError> {
            let row_id = format!("row-{}", self.created.len());
            let chapter_id = data
                .get("chapter_id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned();
            self.created.push((table.to_owned(), data.clone()));
            self.existing
                .insert((table.to_owned(), chapter_id), row_id.clone());
            Ok(row_id)
        }

        fn row_update(
            &mut self,
            table: &str,
            row_id: &str,
            data: &serde_json::Value,
        ) -> Result<(), ProjectionError> {
            self.updated
                .push((table.to_owned(), row_id.to_owned(), data.clone()));
            Ok(())
        }
    }

    fn make_replay(chapter_id: &str, sections: &[(&str, &str)]) -> ChapterReplay {
        let events: Vec<EventRow> = sections
            .iter()
            .enumerate()
            .map(|(i, (section_name, body))| EventRow {
                event_id: EventId(format!("evt-{i}")),
                event_type: "section_append".to_owned(),
                section_name: Some((*section_name).to_owned()),
                payload: serde_json::json!({ "body": body }).to_string(),
                previous_id: None,
                created_at: 1_000_000_000 + i as i64,
            })
            .collect();
        ChapterReplay {
            meta: ChapterMeta {
                chapter_id: ChapterId(chapter_id.to_owned()),
                schema_id: "journal-mcp-canonical-v1".to_owned(),
                current_state: "closed".to_owned(),
                opened_at: 1_000_000_000,
                closed_at: Some(1_000_000_500),
            },
            events,
        }
    }

    /// T1 (boundary) — fresh chapter triggers `row_create` (existing = None
    /// path) and `schema_ensure` is called exactly once.
    #[test]
    fn test_rebuild_calls_row_create_and_schema_ensure_once() {
        let client = MockMiniAppClient::default();
        let mut proj = MiniAppProjection::new(client, MiniAppConfig::default());
        let replay = make_replay("chapter-1", &[("Verified", "alpha-body")]);

        proj.rebuild_chapter(&replay).expect("rebuild 1");
        assert_eq!(
            proj.client.schema_calls.len(),
            1,
            "schema_ensure on first rebuild"
        );
        assert_eq!(proj.client.created.len(), 1);
        assert_eq!(proj.client.updated.len(), 0);

        // Second rebuild does not call schema_ensure again (idempotent at
        // the projection level).
        let replay2 = make_replay("chapter-2", &[("Verified", "beta-body")]);
        proj.rebuild_chapter(&replay2).expect("rebuild 2");
        assert_eq!(
            proj.client.schema_calls.len(),
            1,
            "schema_ensure must not be called again"
        );
        assert_eq!(proj.client.created.len(), 2);
    }

    /// T2 (boundary) — existing chapter triggers `row_update`.
    #[test]
    fn test_rebuild_calls_row_update_for_existing_chapter() {
        let client = MockMiniAppClient::default().with_existing(
            "journal_chapter",
            "chapter-3",
            "row-existing",
        );
        let mut proj = MiniAppProjection::new(client, MiniAppConfig::default());
        let replay = make_replay("chapter-3", &[("Verified", "gamma-body")]);

        proj.rebuild_chapter(&replay).expect("rebuild");
        assert_eq!(proj.client.created.len(), 0);
        assert_eq!(proj.client.updated.len(), 1);
        let (table, row_id, data) = &proj.client.updated[0];
        assert_eq!(table, "journal_chapter");
        assert_eq!(row_id, "row-existing");
        assert_eq!(data["chapter_id"], "chapter-3");
    }

    /// T3 (property) — payload contains the decided_summary extracted
    /// from the `Decided` section's first non-empty line.
    #[test]
    fn test_decided_summary_extraction() {
        let client = MockMiniAppClient::default();
        let mut proj = MiniAppProjection::new(client, MiniAppConfig::default());
        let replay = make_replay(
            "chapter-4",
            &[(
                "Decided",
                "- the first decisive line is the summary\n- second line ignored",
            )],
        );

        proj.rebuild_chapter(&replay).expect("rebuild");
        let (_table, data) = &proj.client.created[0];
        assert_eq!(
            data["decided_summary"],
            "- the first decisive line is the summary"
        );
    }

    /// T4 (property) — issue_refs extracts canonical UUIDs from the
    /// `Issues touched` section body.
    #[test]
    fn test_issue_refs_extraction() {
        let client = MockMiniAppClient::default();
        let mut proj = MiniAppProjection::new(client, MiniAppConfig::default());
        let replay = make_replay(
            "chapter-5",
            &[(
                "Issues touched",
                "- a7cea6d7-1234-4abc-9def-0123456789ab — first issue\n\
                 - 98123835-f79f-47c6-9490-af31ff5665ec — second issue",
            )],
        );

        proj.rebuild_chapter(&replay).expect("rebuild");
        let (_table, data) = &proj.client.created[0];
        let refs: Vec<String> = data["issue_refs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect();
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0], "a7cea6d7-1234-4abc-9def-0123456789ab");
        assert_eq!(refs[1], "98123835-f79f-47c6-9490-af31ff5665ec");
    }

    /// T5 (property) — `mark_dirty` queues + `rebuild_chapter` clears the
    /// dirty entry.
    #[test]
    fn test_mark_dirty_then_rebuild_clears_dirty() {
        let client = MockMiniAppClient::default();
        let mut proj = MiniAppProjection::new(client, MiniAppConfig::default());
        let id = ChapterId("chapter-6".to_owned());
        proj.mark_dirty(&id).expect("mark");
        assert!(proj.dirty_chapters().contains("chapter-6"));

        let replay = make_replay("chapter-6", &[("Verified", "delta")]);
        proj.rebuild_chapter(&replay).expect("rebuild");
        assert!(!proj.dirty_chapters().contains("chapter-6"));
    }

    /// T6 (property) — re-rebuilding the same chapter routes to
    /// `row_update`, not duplicate `row_create`.
    #[test]
    fn test_rebuild_idempotent_routes_second_call_to_update() {
        let client = MockMiniAppClient::default();
        let mut proj = MiniAppProjection::new(client, MiniAppConfig::default());
        let replay = make_replay("chapter-7", &[("Verified", "epsilon")]);

        proj.rebuild_chapter(&replay).expect("first");
        proj.rebuild_chapter(&replay).expect("second");
        assert_eq!(proj.client.created.len(), 1);
        assert_eq!(proj.client.updated.len(), 1);
    }

    /// T7 (boundary) — custom config (table_name + project_label) is
    /// forwarded verbatim.
    #[test]
    fn test_custom_config_forwarded() {
        let client = MockMiniAppClient::default();
        let config = MiniAppConfig {
            table_name: "my_table".to_owned(),
            project_label: "my_project".to_owned(),
        };
        let mut proj = MiniAppProjection::new(client, config);
        let replay = make_replay("chapter-8", &[("Verified", "zeta")]);
        proj.rebuild_chapter(&replay).expect("rebuild");

        let (table, data) = &proj.client.created[0];
        assert_eq!(table, "my_table");
        assert_eq!(data["project_label"], "my_project");
    }

    /// T8 (property) — `scan_uuids` rejects non-UUID hex runs (no false
    /// positives on commit hashes or other hex strings).
    #[test]
    fn test_scan_uuids_no_false_positives() {
        // Commit hash (40 hex chars, no dashes) — must not match.
        let no_match = scan_uuids("commit 53d48c6817c2aaaadeadbeefcafef00d12345678 see also");
        assert!(no_match.is_empty(), "no false positives; got: {no_match:?}");

        // Hex chunks separated by hyphens but with wrong widths — must
        // not match.
        let wrong_widths =
            scan_uuids("ab12-3456-7890-1234-56789012345 (5 groups but wrong widths)");
        assert!(
            wrong_widths.is_empty(),
            "wrong widths must not match; got: {wrong_widths:?}"
        );
    }
}
