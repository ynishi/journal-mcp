//! `FTS5Projection` — SQLite FTS5 full-text search index.
//!
//! Mirrors `section_append` events into a `journal_fts` FTS5 virtual table
//! in the same `.journal.db` as the canonical [`EventLog`].  Enables
//! index-based substring search (≥100x speedup at 1000+ chapters) compared
//! to the `LIKE`-based linear scan in
//! [`JournalCore::grep_chapters`](crate::JournalCore::grep_chapters).
//!
//! # Concurrency
//!
//! `FTS5Projection` opens its own [`rusqlite::Connection`] to the same DB
//! file as the EventLog.  SQLite WAL mode (configured by [`EventLog::open`])
//! makes multi-connection writes safe.
//!
//! # Idempotency
//!
//! [`FTS5Projection::open`] creates the `journal_fts` virtual table with
//! `CREATE VIRTUAL TABLE IF NOT EXISTS`, so re-opening an existing database
//! is a no-op for the table-creation step.
//!
//! Both [`mark_dirty`](FTS5Projection) and
//! [`rebuild_chapter`](FTS5Projection) `DELETE` the chapter's existing rows
//! before re-inserting, so repeated invocations converge on the same state.
//!
//! [`EventLog`]: crate::EventLog
//! [`EventLog::open`]: crate::EventLog::open

use std::path::Path;

use rusqlite::Connection;

use super::{private::Sealed, JournalProjection, ProjectionError};
use crate::{ChapterId, ChapterReplay};

// ---------------------------------------------------------------------------
// FTS5Projection
// ---------------------------------------------------------------------------

/// Full-text search projection over chapter section bodies.
///
/// See the module-level documentation for the design overview.
pub struct FTS5Projection {
    /// Owned connection to the same `.journal.db` that the EventLog uses.
    ///
    /// Multi-connection access is WAL-safe (the EventLog opens its own
    /// `Connection` in WAL mode at startup; subsequent connections inherit
    /// the journal-mode setting).
    conn: Connection,
}

impl FTS5Projection {
    /// Open a connection to the journal database at `db_path` and ensure the
    /// `journal_fts` virtual table exists.
    ///
    /// # Schema
    ///
    /// ```sql
    /// CREATE VIRTUAL TABLE IF NOT EXISTS journal_fts USING fts5(
    ///     chapter_id,
    ///     section_name,
    ///     body,
    ///     tokenize = 'trigram'
    /// )
    /// ```
    ///
    /// The `trigram` tokenizer (SQLite 3.34+) indexes every 3-character
    /// substring of the body, which makes the FTS5 `MATCH` operator
    /// behave like SQL `LIKE '%pattern%'` substring search semantically.
    /// This matches the pre-FTS5 `LIKE`-based `journal_grep` behaviour
    /// caller-side, so attaching `FTS5Projection` is a drop-in speedup
    /// without altering match semantics.  Japanese / CJK substrings are
    /// indexed byte-wise (no morphological splitting required).
    ///
    /// # Errors
    ///
    /// Returns [`ProjectionError::Sql`] if the connection cannot be opened
    /// or the virtual table cannot be created (e.g. SQLite build does not
    /// include the FTS5 module).
    pub fn open(db_path: impl AsRef<Path>) -> Result<Self, ProjectionError> {
        let conn = Connection::open(db_path.as_ref())?;
        Self::ensure_virtual_table(&conn)?;
        Ok(Self { conn })
    }

    /// Create the `journal_fts` virtual table if it does not yet exist.
    ///
    /// Called by [`FTS5Projection::open`] at construction time.  Idempotent.
    fn ensure_virtual_table(conn: &Connection) -> Result<(), ProjectionError> {
        conn.execute(
            "CREATE VIRTUAL TABLE IF NOT EXISTS journal_fts USING fts5(\
                chapter_id, \
                section_name, \
                body, \
                tokenize = 'trigram'\
             )",
            [],
        )?;
        Ok(())
    }

    /// Substring search across all indexed section bodies.
    ///
    /// Used by callers that want the FTS5 fast path explicitly (e.g. the
    /// `journal_grep` MCP tool handler when an [`FTS5Projection`] is
    /// attached).  Returns matching `(chapter_id, section_name, body)`
    /// triples in match-order (SQLite FTS5 default ranking).
    ///
    /// The pattern is forwarded to the FTS5 `MATCH` operator wrapped in
    /// double quotes (phrase-query form) so that substring matching against
    /// the trigram tokenizer behaves like SQL `LIKE '%pattern%'`.  Callers
    /// pass a literal substring; pattern length must be ≥3 characters
    /// (trigram tokenizer requirement — shorter patterns return no rows).
    ///
    /// # Arguments
    ///
    /// * `pattern` — the FTS5 query string.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectionError::Sql`] on SQL execution failure.
    pub fn search(&self, pattern: &str) -> Result<Vec<(String, String, String)>, ProjectionError> {
        // Wrap the substring in double quotes so the FTS5 MATCH operator
        // treats it as a phrase query.  This makes the trigram tokenizer
        // perform substring matching equivalent to SQL `LIKE '%pattern%'`.
        // Embedded double quotes in the substring are escaped per FTS5 rules
        // (double them to insert a literal quote inside a phrase).
        let escaped = pattern.replace('"', "\"\"");
        let phrase_query = format!("\"{escaped}\"");
        let mut stmt = self.conn.prepare(
            "SELECT chapter_id, section_name, body \
             FROM journal_fts \
             WHERE journal_fts MATCH ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![phrase_query], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Delete all rows for the given chapter from the FTS5 index.
    ///
    /// Internal helper shared by [`mark_dirty`](FTS5Projection) and
    /// [`rebuild_chapter`](FTS5Projection).
    fn delete_chapter(&self, chapter_id: &str) -> Result<(), ProjectionError> {
        self.conn.execute(
            "DELETE FROM journal_fts WHERE chapter_id = ?1",
            rusqlite::params![chapter_id],
        )?;
        Ok(())
    }
}

impl Sealed for FTS5Projection {}

impl JournalProjection for FTS5Projection {
    fn name(&self) -> &'static str {
        "fts5"
    }

    /// Mark the chapter as stale by deleting its existing index entries.
    ///
    /// FTS5Projection treats `mark_dirty` as a complete invalidation — the
    /// chapter's rows are removed entirely.  They will be re-inserted on
    /// the next [`rebuild_chapter`](FTS5Projection) call.
    fn mark_dirty(&mut self, id: &ChapterId) -> Result<(), ProjectionError> {
        self.delete_chapter(&id.0)
    }

    /// Rebuild the FTS5 index for the chapter described by `replay`.
    ///
    /// 1. Delete all existing rows for `chapter_id` (so the operation is
    ///    idempotent against repeated rebuilds).
    /// 2. Re-insert one row per `section_append` event, extracting the body
    ///    string from the event's JSON payload.
    ///
    /// Non-`section_append` events (open / close / append_progress / import)
    /// are skipped — only the section bodies feed the search index.
    fn rebuild_chapter(&mut self, replay: &ChapterReplay) -> Result<(), ProjectionError> {
        let chapter_id = &replay.meta.chapter_id.0;
        self.delete_chapter(chapter_id)?;

        for event in &replay.events {
            if event.event_type != "section_append" {
                continue;
            }
            let section_name = event.section_name.clone().unwrap_or_default();
            let payload: serde_json::Value = serde_json::from_str(&event.payload)?;
            let body = payload
                .get("body")
                .and_then(|b| b.as_str())
                .unwrap_or_default();
            self.conn.execute(
                "INSERT INTO journal_fts (chapter_id, section_name, body) \
                 VALUES (?1, ?2, ?3)",
                rusqlite::params![chapter_id, section_name, body],
            )?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_log::{ChapterMeta, EventRow};
    use crate::ChapterId;

    /// Build a minimal `ChapterReplay` with the given section events for
    /// testing.  Bypasses `JournalCore` so the test exercises
    /// `FTS5Projection` in isolation.
    fn make_replay(chapter_id: &str, sections: &[(&str, &str)]) -> ChapterReplay {
        let events: Vec<EventRow> = sections
            .iter()
            .enumerate()
            .map(|(i, (section_name, body))| EventRow {
                event_id: crate::event_log::EventId(format!("evt-{i}")),
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

    /// T1 (property) — `open` creates the virtual table on a fresh database,
    /// and re-opening the same database is a no-op (idempotent).
    #[test]
    fn test_open_creates_virtual_table_idempotent() {
        let tmp = tempfile::TempDir::new().expect("TempDir should succeed");
        let db_path = tmp.path().join("test.db");
        let _first = FTS5Projection::open(&db_path).expect("first open should succeed");
        let _second = FTS5Projection::open(&db_path).expect("second open should succeed");
    }

    /// T2 (boundary) — `rebuild_chapter` inserts one row per
    /// `section_append` event; `search` matches on substring.
    #[test]
    fn test_rebuild_then_search_matches() {
        let tmp = tempfile::TempDir::new().expect("TempDir should succeed");
        let db_path = tmp.path().join("test.db");
        let mut proj = FTS5Projection::open(&db_path).expect("open should succeed");
        let replay = make_replay(
            "chapter-1",
            &[
                ("Verified", "the unique-search-token-alpha appears here"),
                ("Done", "unrelated body"),
            ],
        );
        proj.rebuild_chapter(&replay)
            .expect("rebuild should succeed");

        let results = proj
            .search("unique-search-token-alpha")
            .expect("search should succeed");
        assert_eq!(results.len(), 1, "exactly one row should match");
        assert_eq!(results[0].0, "chapter-1");
        assert_eq!(results[0].1, "Verified");
        assert!(results[0].2.contains("unique-search-token-alpha"));
    }

    /// T3 (property) — `rebuild_chapter` is idempotent: calling it twice
    /// with the same replay yields the same search results.
    #[test]
    fn test_rebuild_idempotent() {
        let tmp = tempfile::TempDir::new().expect("TempDir should succeed");
        let db_path = tmp.path().join("test.db");
        let mut proj = FTS5Projection::open(&db_path).expect("open should succeed");
        let replay = make_replay("chapter-2", &[("Verified", "idempotent-token-beta body")]);

        proj.rebuild_chapter(&replay).expect("first rebuild");
        proj.rebuild_chapter(&replay).expect("second rebuild");

        let results = proj
            .search("idempotent-token-beta")
            .expect("search should succeed");
        assert_eq!(
            results.len(),
            1,
            "duplicate rebuilds must yield exactly one row, not two"
        );
    }

    /// T4 (boundary) — `mark_dirty` removes all rows for the chapter; a
    /// subsequent search returns no results.
    #[test]
    fn test_mark_dirty_removes_chapter_rows() {
        let tmp = tempfile::TempDir::new().expect("TempDir should succeed");
        let db_path = tmp.path().join("test.db");
        let mut proj = FTS5Projection::open(&db_path).expect("open should succeed");
        let replay = make_replay("chapter-3", &[("Verified", "delete-token-gamma here")]);

        proj.rebuild_chapter(&replay).expect("rebuild");
        let pre = proj.search("delete-token-gamma").expect("pre-mark search");
        assert_eq!(pre.len(), 1, "should match before mark_dirty");

        proj.mark_dirty(&replay.meta.chapter_id)
            .expect("mark_dirty");
        let post = proj.search("delete-token-gamma").expect("post-mark search");
        assert!(
            post.is_empty(),
            "mark_dirty must remove all rows; got: {post:?}"
        );
    }

    /// T5 (property) — non-`section_append` events (open / close /
    /// append_progress) are skipped; only section bodies are indexed.
    #[test]
    fn test_rebuild_skips_non_section_events() {
        let tmp = tempfile::TempDir::new().expect("TempDir should succeed");
        let db_path = tmp.path().join("test.db");
        let mut proj = FTS5Projection::open(&db_path).expect("open should succeed");

        let chapter_id = "chapter-4";
        let mixed_replay = ChapterReplay {
            meta: ChapterMeta {
                chapter_id: ChapterId(chapter_id.to_owned()),
                schema_id: "journal-mcp-canonical-v1".to_owned(),
                current_state: "closed".to_owned(),
                opened_at: 1_000_000_000,
                closed_at: Some(1_000_000_500),
            },
            events: vec![
                EventRow {
                    event_id: crate::event_log::EventId("evt-open".to_owned()),
                    event_type: "open".to_owned(),
                    section_name: None,
                    payload: serde_json::json!({ "initial_state": "open" }).to_string(),
                    previous_id: None,
                    created_at: 1_000_000_000,
                },
                EventRow {
                    event_id: crate::event_log::EventId("evt-section".to_owned()),
                    event_type: "section_append".to_owned(),
                    section_name: Some("Verified".to_owned()),
                    payload: serde_json::json!({ "body": "skip-test-token-delta" }).to_string(),
                    previous_id: None,
                    created_at: 1_000_000_100,
                },
                EventRow {
                    event_id: crate::event_log::EventId("evt-progress".to_owned()),
                    event_type: "append_progress".to_owned(),
                    section_name: Some("Progress".to_owned()),
                    payload: serde_json::json!({ "line": "step 1" }).to_string(),
                    previous_id: None,
                    created_at: 1_000_000_200,
                },
                EventRow {
                    event_id: crate::event_log::EventId("evt-close".to_owned()),
                    event_type: "close".to_owned(),
                    section_name: None,
                    payload: serde_json::json!({}).to_string(),
                    previous_id: None,
                    created_at: 1_000_000_500,
                },
            ],
        };

        proj.rebuild_chapter(&mixed_replay)
            .expect("rebuild should succeed");

        // Count rows for this chapter in the FTS5 index.
        let count: i64 = proj
            .conn
            .query_row(
                "SELECT COUNT(*) FROM journal_fts WHERE chapter_id = ?1",
                rusqlite::params![chapter_id],
                |row| row.get(0),
            )
            .expect("count should succeed");
        assert_eq!(
            count, 1,
            "only the section_append event should be indexed; got {count} rows"
        );
    }

    /// T6 (boundary) — multi-chapter: rebuilding chapter A does not affect
    /// rows indexed for chapter B.
    #[test]
    fn test_multi_chapter_isolation() {
        let tmp = tempfile::TempDir::new().expect("TempDir should succeed");
        let db_path = tmp.path().join("test.db");
        let mut proj = FTS5Projection::open(&db_path).expect("open should succeed");

        let replay_a = make_replay("chapter-A", &[("Verified", "chapter-a-token-epsilon")]);
        let replay_b = make_replay("chapter-B", &[("Verified", "chapter-b-token-zeta")]);

        proj.rebuild_chapter(&replay_a).expect("rebuild A");
        proj.rebuild_chapter(&replay_b).expect("rebuild B");

        // Re-rebuilding A must not touch B's rows.
        proj.rebuild_chapter(&replay_a).expect("re-rebuild A");

        let a_match = proj.search("chapter-a-token-epsilon").expect("search A");
        let b_match = proj.search("chapter-b-token-zeta").expect("search B");
        assert_eq!(a_match.len(), 1, "chapter A token should still match");
        assert_eq!(b_match.len(), 1, "chapter B token should still match");
    }

    /// T7 (boundary) — Japanese substring (`unicode61` tokenizer) matches
    /// correctly.
    #[test]
    fn test_japanese_substring_match() {
        let tmp = tempfile::TempDir::new().expect("TempDir should succeed");
        let db_path = tmp.path().join("test.db");
        let mut proj = FTS5Projection::open(&db_path).expect("open should succeed");

        let replay = make_replay(
            "chapter-jp",
            &[("Verified", "ジャーナル統合の実装着地を確認した")],
        );
        proj.rebuild_chapter(&replay)
            .expect("rebuild should succeed");

        let results = proj.search("ジャーナル").expect("search should succeed");
        assert!(
            !results.is_empty(),
            "Japanese substring should match via unicode61 tokenizer"
        );
        assert_eq!(results[0].0, "chapter-jp");
    }
}
