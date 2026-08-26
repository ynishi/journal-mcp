//! `SqliteVectorProjection` — persistent variant of [`VectorProjection`]
//! backed by a plain SQLite `embedding BLOB` column.
//!
//! # ε-2 (plain SQLite BLOB store)
//!
//! Stores embeddings in a regular SQLite table (no virtual-table
//! extension) so search results survive restarts.  The cost is a
//! `O(N)` linear scan per query (loading every stored vector and
//! computing cosine similarity in-process) — acceptable at the
//! 100-1000 chapter scale typical of project canonical histories.
//!
//! When the upstream `sqlite-vec` extension stabilizes (currently
//! 0.1.10-alpha.4 with macOS arm64 build issues), a separate
//! `SqliteVecVectorProjection` backend can land alongside this one
//! without breaking the existing API surface.
//!
//! # Storage schema
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS journal_vec_embeddings (
//!     chapter_id TEXT PRIMARY KEY,
//!     embedding  BLOB NOT NULL  -- f32 little-endian, 4 bytes per dimension
//! )
//! ```
//!
//! # Concurrency
//!
//! Like [`super::fts5::FTS5Projection`], this projection opens its own
//! `rusqlite::Connection` to the same `.journal.db` as the EventLog;
//! SQLite WAL mode (configured by `EventLog::open`) makes multi-connection
//! writes safe.

use std::path::Path;

use rusqlite::Connection;

use super::vector::{cosine_similarity, VectorClient, VectorConfig};
use super::{private::Sealed, JournalProjection, ProjectionError};
use crate::{ChapterId, ChapterReplay};

// ---------------------------------------------------------------------------
// SqliteVectorProjection
// ---------------------------------------------------------------------------

/// Persistent semantic search projection storing embeddings in a plain
/// SQLite `BLOB` column.  Generic over the [`VectorClient`] embedder.
pub struct SqliteVectorProjection<C: VectorClient> {
    /// Embedder client.
    client: C,
    /// Static configuration (embedding dimension).
    config: VectorConfig,
    /// Owned connection to the same `.journal.db` that the EventLog uses.
    conn: Connection,
}

impl<C: VectorClient> SqliteVectorProjection<C> {
    /// Open a connection to `db_path`, ensure the embedding table
    /// exists, and return a ready-to-use projection.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectionError::Sql`] on SQLite open / table-create
    /// failure.
    pub fn open(
        db_path: impl AsRef<Path>,
        client: C,
        config: VectorConfig,
    ) -> Result<Self, ProjectionError> {
        let conn = Connection::open(db_path.as_ref())?;
        Self::ensure_table(&conn)?;
        Ok(Self {
            client,
            config,
            conn,
        })
    }

    /// Create the `journal_vec_embeddings` table if it does not yet
    /// exist.  Idempotent.
    fn ensure_table(conn: &Connection) -> Result<(), ProjectionError> {
        conn.execute(
            "CREATE TABLE IF NOT EXISTS journal_vec_embeddings (\
                chapter_id TEXT PRIMARY KEY, \
                embedding  BLOB NOT NULL\
             )",
            [],
        )?;
        Ok(())
    }

    /// Count of rows currently stored.  Used by tests to inspect state.
    pub fn count(&self) -> Result<i64, ProjectionError> {
        let n = self
            .conn
            .query_row("SELECT COUNT(*) FROM journal_vec_embeddings", [], |r| {
                r.get(0)
            })?;
        Ok(n)
    }

    /// Fetch the stored vector for `chapter_id`, decoding the BLOB into
    /// a `Vec<f32>`.  Returns `Ok(None)` if no row matches.  Used by
    /// tests to assert round-trip fidelity.
    pub fn fetch_embedding(&self, chapter_id: &str) -> Result<Option<Vec<f32>>, ProjectionError> {
        let blob: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT embedding FROM journal_vec_embeddings WHERE chapter_id = ?1",
                rusqlite::params![chapter_id],
                |r| r.get(0),
            )
            .ok();
        Ok(blob.map(|b| decode_vec(&b)))
    }

    /// Semantic search: embed `query` then linear-scan every stored
    /// embedding for cosine similarity, returning the top-`limit`
    /// chapter IDs sorted descending.
    ///
    /// # Errors
    ///
    /// - [`ProjectionError::Sql`] on SQLite read failure.
    /// - [`ProjectionError::Io`] from the embedder, or on query-vector
    ///   dimension mismatch against [`VectorConfig::dimension`].
    pub fn search(
        &mut self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(String, f32)>, ProjectionError> {
        let query_vec = self.client.embed(query)?;
        if query_vec.len() != self.config.dimension {
            return Err(ProjectionError::Io(std::io::Error::other(format!(
                "query embedding dimension mismatch: expected {}, got {}",
                self.config.dimension,
                query_vec.len()
            ))));
        }
        let mut stmt = self
            .conn
            .prepare("SELECT chapter_id, embedding FROM journal_vec_embeddings")?;
        let rows = stmt.query_map([], |row| {
            let id: String = row.get(0)?;
            let blob: Vec<u8> = row.get(1)?;
            Ok((id, blob))
        })?;
        let mut scored: Vec<(String, f32)> = Vec::new();
        for r in rows {
            let (id, blob) = r?;
            let v = decode_vec(&blob);
            let score = cosine_similarity(&query_vec, &v);
            scored.push((id, score));
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);
        Ok(scored)
    }

    /// Concatenate section bodies into a single text for embedding.
    /// Skips non-`section_append` events.
    fn render_text(replay: &ChapterReplay) -> Result<String, ProjectionError> {
        let mut out = String::new();
        for event in &replay.events {
            if event.event_type != "section_append" {
                continue;
            }
            let payload: serde_json::Value = serde_json::from_str(&event.payload)?;
            let body = payload
                .get("body")
                .and_then(|b| b.as_str())
                .unwrap_or_default();
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(body.trim());
        }
        Ok(out)
    }
}

impl<C: VectorClient> Sealed for SqliteVectorProjection<C> {}

impl<C: VectorClient + 'static> JournalProjection for SqliteVectorProjection<C> {
    fn name(&self) -> &'static str {
        "vector-sqlite"
    }

    /// Delete the chapter's stored embedding so the next
    /// `rebuild_chapter` re-inserts it.
    fn mark_dirty(&mut self, id: &ChapterId) -> Result<(), ProjectionError> {
        self.conn.execute(
            "DELETE FROM journal_vec_embeddings WHERE chapter_id = ?1",
            rusqlite::params![id.0],
        )?;
        Ok(())
    }

    /// Compute the embedding for the chapter's concatenated section
    /// bodies and `INSERT OR REPLACE` it into the table.
    fn rebuild_chapter(&mut self, replay: &ChapterReplay) -> Result<(), ProjectionError> {
        let chapter_id = replay.meta.chapter_id.0.clone();
        let text = Self::render_text(replay)?;
        let vec = self.client.embed(&text)?;
        if vec.len() != self.config.dimension {
            return Err(ProjectionError::Io(std::io::Error::other(format!(
                "embedding dimension mismatch for chapter {chapter_id}: expected {}, got {}",
                self.config.dimension,
                vec.len()
            ))));
        }
        let blob = encode_vec(&vec);
        self.conn.execute(
            "INSERT OR REPLACE INTO journal_vec_embeddings (chapter_id, embedding) \
             VALUES (?1, ?2)",
            rusqlite::params![chapter_id, blob],
        )?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// f32 ↔ BLOB encoding
// ---------------------------------------------------------------------------

/// Encode a `&[f32]` as a `Vec<u8>` using little-endian 4-byte words.
///
/// Little-endian is chosen so the encoding matches the in-memory
/// representation on all common SQLite host architectures (x86_64,
/// arm64) — the table is therefore architecture-portable but bit-exact
/// on the platforms we care about.
fn encode_vec(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

/// Decode a `&[u8]` BLOB (little-endian 4-byte f32 words) back into a
/// `Vec<f32>`.  Trailing bytes that do not form a complete f32 are
/// silently dropped — callers ensure the BLOB was produced by
/// [`encode_vec`], so this should never occur in practice.
fn decode_vec(blob: &[u8]) -> Vec<f32> {
    blob.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_log::{ChapterMeta, EventId, EventRow};

    /// `VectorClient` that returns a configured fixed vector.
    struct FixedEmbedder {
        vector: Vec<f32>,
    }
    impl VectorClient for FixedEmbedder {
        fn embed(&mut self, _text: &str) -> Result<Vec<f32>, ProjectionError> {
            Ok(self.vector.clone())
        }
    }

    /// `VectorClient` that returns a wrong-dimension vector (for the
    /// mismatch-error test).
    struct WrongDimensionEmbedder;
    impl VectorClient for WrongDimensionEmbedder {
        fn embed(&mut self, _text: &str) -> Result<Vec<f32>, ProjectionError> {
            Ok(vec![1.0, 0.0])
        }
    }

    fn cfg_3d() -> VectorConfig {
        VectorConfig { dimension: 3 }
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
                chapter_name: None,
            },
            events,
        }
    }

    /// T1 (boundary) — `rebuild_chapter` stores the embedding and
    /// `fetch_embedding` round-trips an identical `Vec<f32>`.
    #[test]
    fn test_rebuild_round_trip_via_blob() {
        let tmp = tempfile::TempDir::new().expect("TempDir should succeed");
        let db_path = tmp.path().join("vec.db");
        let embedder = FixedEmbedder {
            vector: vec![0.1_f32, -0.2, 0.3],
        };
        let mut proj = SqliteVectorProjection::open(&db_path, embedder, cfg_3d())
            .expect("open should succeed");

        let replay = make_replay("chapter-1", &[("Verified", "any body")]);
        proj.rebuild_chapter(&replay).expect("rebuild");
        assert_eq!(proj.count().unwrap(), 1);

        let fetched = proj
            .fetch_embedding("chapter-1")
            .expect("fetch")
            .expect("Some");
        assert_eq!(fetched.len(), 3);
        // Bit-exact round trip — encode/decode preserves f32 bytes.
        assert_eq!(fetched, vec![0.1_f32, -0.2, 0.3]);
    }

    /// T2 (property) — persistence: re-opening the same DB picks up
    /// previously stored embeddings.
    #[test]
    fn test_embeddings_persist_across_open() {
        let tmp = tempfile::TempDir::new().expect("TempDir should succeed");
        let db_path = tmp.path().join("vec.db");

        {
            let embedder = FixedEmbedder {
                vector: vec![1.0_f32, 0.0, 0.0],
            };
            let mut proj =
                SqliteVectorProjection::open(&db_path, embedder, cfg_3d()).expect("open 1");
            let replay = make_replay("chapter-persist", &[("Verified", "x")]);
            proj.rebuild_chapter(&replay).expect("rebuild");
            assert_eq!(proj.count().unwrap(), 1);
        }
        // First instance dropped, connection closed; re-open.
        let embedder = FixedEmbedder {
            vector: vec![0.0; 3],
        };
        let proj = SqliteVectorProjection::open(&db_path, embedder, cfg_3d()).expect("open 2");
        assert_eq!(
            proj.count().unwrap(),
            1,
            "stored embedding must survive close + reopen"
        );
        let fetched = proj.fetch_embedding("chapter-persist").unwrap().unwrap();
        assert_eq!(fetched, vec![1.0_f32, 0.0, 0.0]);
    }

    /// T3 (property) — re-rebuilding the same chapter replaces (not
    /// duplicates) its row.
    #[test]
    fn test_rebuild_replaces_existing_row() {
        let tmp = tempfile::TempDir::new().expect("TempDir should succeed");
        let db_path = tmp.path().join("vec.db");
        let embedder = FixedEmbedder {
            vector: vec![1.0, 2.0, 3.0],
        };
        let mut proj = SqliteVectorProjection::open(&db_path, embedder, cfg_3d())
            .expect("open should succeed");
        let replay_v1 = make_replay("chapter-2", &[("Verified", "v1")]);
        let replay_v2 = make_replay("chapter-2", &[("Verified", "v2")]);
        proj.rebuild_chapter(&replay_v1).expect("v1");
        proj.rebuild_chapter(&replay_v2).expect("v2");
        assert_eq!(proj.count().unwrap(), 1);
    }

    /// T4 (boundary) — `mark_dirty` deletes the chapter's row.
    #[test]
    fn test_mark_dirty_deletes_row() {
        let tmp = tempfile::TempDir::new().expect("TempDir should succeed");
        let db_path = tmp.path().join("vec.db");
        let embedder = FixedEmbedder {
            vector: vec![1.0, 0.0, 0.0],
        };
        let mut proj = SqliteVectorProjection::open(&db_path, embedder, cfg_3d())
            .expect("open should succeed");
        let replay = make_replay("chapter-3", &[("Verified", "x")]);
        proj.rebuild_chapter(&replay).expect("rebuild");
        assert_eq!(proj.count().unwrap(), 1);
        let id = ChapterId("chapter-3".to_owned());
        proj.mark_dirty(&id).expect("mark");
        assert_eq!(proj.count().unwrap(), 0);
    }

    /// T5 (property) — `search` ranks an exactly-matching vector first.
    #[test]
    fn test_search_ranks_exact_match_first() {
        let tmp = tempfile::TempDir::new().expect("TempDir should succeed");
        let db_path = tmp.path().join("vec.db");
        // Manually populate two distinct embeddings, then search with a
        // FixedEmbedder that always returns [1, 0, 0] — chapter-A
        // (which has the same vector) must rank first.
        let mut proj = SqliteVectorProjection::open(
            &db_path,
            FixedEmbedder {
                vector: vec![1.0, 0.0, 0.0],
            },
            cfg_3d(),
        )
        .expect("open should succeed");

        proj.conn
            .execute(
                "INSERT INTO journal_vec_embeddings (chapter_id, embedding) VALUES (?1, ?2)",
                rusqlite::params!["chapter-A", encode_vec(&[1.0_f32, 0.0, 0.0])],
            )
            .unwrap();
        proj.conn
            .execute(
                "INSERT INTO journal_vec_embeddings (chapter_id, embedding) VALUES (?1, ?2)",
                rusqlite::params!["chapter-B", encode_vec(&[0.0_f32, 1.0, 0.0])],
            )
            .unwrap();

        let results = proj.search("any query", 2).expect("search");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, "chapter-A");
        assert!(
            (results[0].1 - 1.0).abs() < 1e-6,
            "exact match must score ~1.0; got {}",
            results[0].1
        );
        assert!(
            results[1].1.abs() < 1e-6,
            "orthogonal vector must score ~0.0; got {}",
            results[1].1
        );
    }

    /// T6 (boundary) — `search` respects the `limit` argument.
    #[test]
    fn test_search_respects_limit() {
        let tmp = tempfile::TempDir::new().expect("TempDir should succeed");
        let db_path = tmp.path().join("vec.db");
        let mut proj = SqliteVectorProjection::open(
            &db_path,
            FixedEmbedder {
                vector: vec![1.0, 0.0, 0.0],
            },
            cfg_3d(),
        )
        .expect("open should succeed");
        for i in 0..5 {
            proj.conn
                .execute(
                    "INSERT INTO journal_vec_embeddings (chapter_id, embedding) VALUES (?1, ?2)",
                    rusqlite::params![format!("chapter-{i}"), encode_vec(&[1.0_f32, 0.0, 0.0])],
                )
                .unwrap();
        }
        let results = proj.search("query", 2).expect("search");
        assert_eq!(results.len(), 2);
    }

    /// T7 (boundary) — `rebuild_chapter` errors on dimension mismatch.
    #[test]
    fn test_rebuild_errors_on_dimension_mismatch() {
        let tmp = tempfile::TempDir::new().expect("TempDir should succeed");
        let db_path = tmp.path().join("vec.db");
        let mut proj = SqliteVectorProjection::open(&db_path, WrongDimensionEmbedder, cfg_3d())
            .expect("open should succeed");
        let replay = make_replay("chapter-bad", &[("Verified", "x")]);
        let err = proj.rebuild_chapter(&replay).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("dimension mismatch"),
            "error must mention dimension mismatch; got: {msg}"
        );
    }

    /// T8 (boundary) — `search` errors on query-embedding dimension
    /// mismatch.
    #[test]
    fn test_search_errors_on_query_dimension_mismatch() {
        let tmp = tempfile::TempDir::new().expect("TempDir should succeed");
        let db_path = tmp.path().join("vec.db");
        let mut proj = SqliteVectorProjection::open(&db_path, WrongDimensionEmbedder, cfg_3d())
            .expect("open should succeed");
        let err = proj.search("query", 1).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("dimension mismatch"),
            "error must mention dimension mismatch; got: {msg}"
        );
    }

    /// T9 (property) — stable `name() == "vector-sqlite"`.
    #[test]
    fn test_name_returns_vector_sqlite() {
        let tmp = tempfile::TempDir::new().expect("TempDir should succeed");
        let db_path = tmp.path().join("vec.db");
        let proj = SqliteVectorProjection::open(
            &db_path,
            FixedEmbedder {
                vector: vec![0.0; 3],
            },
            cfg_3d(),
        )
        .expect("open");
        assert_eq!(proj.name(), "vector-sqlite");
    }

    /// T10 (property) — `encode_vec` ↔ `decode_vec` round-trip is
    /// bit-exact for an arbitrary f32 slice.
    #[test]
    fn test_blob_codec_round_trip() {
        let v = vec![0.0_f32, 1.0, -1.0, 1e-30, 1e30, f32::MIN_POSITIVE];
        let blob = encode_vec(&v);
        let decoded = decode_vec(&blob);
        assert_eq!(decoded, v);
    }
}
