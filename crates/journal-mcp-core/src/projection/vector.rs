//! `VectorProjection` — embedding-based semantic search index.
//!
//! Projects each chapter to a fixed-dimension embedding vector that can be
//! retrieved via cosine-similarity search, enabling semantic queries
//! ("find chapters where I decided about X") without exact keyword match.
//!
//! # Design split (ε-1 / ε-2 / ε-3 / ε-4)
//!
//! This module implements **ε-1**: the projection logic + the
//! [`VectorClient`] trait abstraction over the embedding compute path +
//! an in-memory `BTreeMap` vector store + cosine-similarity search.  All
//! embedding-compute and storage operations route through the
//! [`VectorClient`] trait or stay in-process, which makes the projection
//! mock-implementable for testing.
//!
//! Follow-up commits on the same topic branch land:
//!
//! - **ε-2**: persistent `sqlite-vec` virtual table backend replacing the
//!   in-memory store (so search results survive restarts).
//! - **ε-3**: concrete `CandleEmbedder` ([`VectorClient`] impl) using
//!   `candle-core` + `tokenizers` + `hf-hub` to load `all-MiniLM-L6-v2`
//!   locally (Metal acceleration on Apple Silicon).
//! - **ε-4**: 17th MCP tool `journal_semantic_search(query, project_root?,
//!   limit?)` that calls into [`VectorProjection::search`].
//!
//! # Mock-first ε-1
//!
//! At ε-1 the projection is fully functional end-to-end with a mock
//! embedder + in-memory store: callers can drive [`rebuild_chapter`] and
//! [`search`] against any embedding model that produces a fixed-dimension
//! `Vec<f32>`.  ε-2/ε-3/ε-4 swap in production backends without changing
//! the trait surface.

use std::collections::BTreeMap;

use super::{private::Sealed, JournalProjection, ProjectionError};
use crate::{ChapterId, ChapterReplay};

// ---------------------------------------------------------------------------
// VectorClient trait — embedding compute abstraction
// ---------------------------------------------------------------------------

/// Compute a fixed-dimension embedding vector for the given text.
///
/// Implementations route to either a locally-loaded embedding model
/// (ε-3 candle path) or a remote HTTP endpoint (Ollama / vLLM / SGLang).
/// At ε-1 only a `MockEmbedder` exists for unit testing.
///
/// # Contract
///
/// - The returned vector length must equal the dimension configured in
///   the [`VectorConfig`] used at projection construction.  Mismatched
///   dimensions produce [`ProjectionError::Io`] at storage / search time.
/// - The embedder should be deterministic: the same input text must
///   produce the same vector across invocations within a single process.
///   (The model itself may be non-deterministic across releases; that is
///   the caller's concern.)
pub trait VectorClient: Send {
    /// Compute an embedding vector for `text`.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectionError::Io`] on backend failure (network,
    /// model load, tokenization, etc.).
    fn embed(&mut self, text: &str) -> Result<Vec<f32>, ProjectionError>;
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for [`VectorProjection`].
#[derive(Debug, Clone)]
pub struct VectorConfig {
    /// Embedding dimension that the configured [`VectorClient`] produces.
    ///
    /// Stored vectors and query vectors are checked against this value;
    /// mismatched dimensions produce a runtime error.  Common values:
    /// 384 (`all-MiniLM-L6-v2`), 768 (`all-mpnet-base-v2`), 1536
    /// (OpenAI `text-embedding-3-small`).
    pub dimension: usize,
}

impl Default for VectorConfig {
    fn default() -> Self {
        Self {
            // `all-MiniLM-L6-v2`, the default ε-3 model.
            dimension: 384,
        }
    }
}

// ---------------------------------------------------------------------------
// VectorProjection
// ---------------------------------------------------------------------------

/// Embedding-based semantic search projection, generic over the
/// [`VectorClient`] implementation.
///
/// At ε-1 the projection uses an in-memory `BTreeMap` keyed by
/// `chapter_id` for embeddings; ε-2 swaps this for a `sqlite-vec`
/// virtual table without changing the public method surface.
pub struct VectorProjection<C: VectorClient> {
    /// Embedder client.
    client: C,

    /// Static configuration (embedding dimension).
    config: VectorConfig,

    /// In-memory embedding store, keyed by `chapter_id`.
    ///
    /// `BTreeMap` gives deterministic iteration order for tie-breaking
    /// in [`search`](VectorProjection::search) when multiple chapters
    /// score the same cosine similarity.  ε-2 replaces this with a
    /// `sqlite-vec` virtual table.
    embeddings: BTreeMap<String, Vec<f32>>,
}

impl<C: VectorClient> VectorProjection<C> {
    /// Construct a `VectorProjection` with the given embedder client and
    /// config.
    pub fn new(client: C, config: VectorConfig) -> Self {
        Self {
            client,
            config,
            embeddings: BTreeMap::new(),
        }
    }

    /// Return a reference to the in-memory embedding map.
    ///
    /// Used by tests to inspect internal state.
    pub fn embeddings(&self) -> &BTreeMap<String, Vec<f32>> {
        &self.embeddings
    }

    /// Concatenate the chapter's section bodies into a single text for
    /// embedding.  Skips non-`section_append` events (open / close /
    /// append_progress / import) — only section bodies feed the index.
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

    /// Semantic search: embed `query` and return the top-`limit` chapter
    /// IDs ranked by cosine similarity (descending).
    ///
    /// # Arguments
    ///
    /// * `query` — natural-language query text.
    /// * `limit` — maximum number of results.
    ///
    /// # Returns
    ///
    /// A `Vec<(chapter_id, score)>` sorted by `score` descending.  Each
    /// `score` is the cosine similarity (range `[-1.0, 1.0]`, where
    /// `1.0` is a perfect match).
    ///
    /// # Errors
    ///
    /// Returns [`ProjectionError::Io`] if the embedder fails or the
    /// query embedding's dimension does not match
    /// [`VectorConfig::dimension`].
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
        let mut scored: Vec<(String, f32)> = self
            .embeddings
            .iter()
            .map(|(id, v)| (id.clone(), cosine_similarity(&query_vec, v)))
            .collect();
        // Sort by score descending; ties broken by chapter_id ascending
        // (the BTreeMap iteration order, preserved by stable sort).
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);
        Ok(scored)
    }
}

impl<C: VectorClient> Sealed for VectorProjection<C> {}

impl<C: VectorClient + 'static> JournalProjection for VectorProjection<C> {
    fn name(&self) -> &'static str {
        "vector"
    }

    /// `VectorProjection` rebuilds the affected chapter's embedding on
    /// the next [`rebuild_chapter`](VectorProjection::rebuild_chapter)
    /// call, so `mark_dirty` is a no-op (the rebuild covers it
    /// implicitly).
    fn mark_dirty(&mut self, _id: &ChapterId) -> Result<(), ProjectionError> {
        Ok(())
    }

    /// Compute the embedding for the chapter's concatenated section
    /// bodies and store (or replace) it in the in-memory map.
    ///
    /// # Errors
    ///
    /// - [`ProjectionError::Json`] if a section payload is malformed.
    /// - [`ProjectionError::Io`] if the embedder fails or the embedding
    ///   dimension does not match [`VectorConfig::dimension`].
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
        self.embeddings.insert(chapter_id, vec);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Cosine similarity
// ---------------------------------------------------------------------------

/// Compute cosine similarity between two equal-length vectors.
///
/// Both vectors are assumed to have the same length and to contain at
/// least one non-zero component; degenerate inputs (length mismatch or
/// all-zero vectors) return `0.0` rather than producing NaN.
///
/// `cos_sim(a, b) = (a · b) / (||a|| * ||b||)` ∈ `[-1.0, 1.0]`.
pub(super) fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0_f32;
    let mut norm_a = 0.0_f32;
    let mut norm_b = 0.0_f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        norm_a += a[i] * a[i];
        norm_b += b[i] * b[i];
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a.sqrt() * norm_b.sqrt())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_log::{ChapterMeta, EventId, EventRow};

    /// Mock embedder that returns deterministic 3-dimensional vectors
    /// based on a simple character-hash so the tests can assert on the
    /// resulting cosine similarities.
    ///
    /// Length 3 chosen (instead of the production 384) to keep the
    /// test assertions readable.
    #[derive(Default)]
    struct MockEmbedder {
        embed_calls: Vec<String>,
    }

    impl VectorClient for MockEmbedder {
        fn embed(&mut self, text: &str) -> Result<Vec<f32>, ProjectionError> {
            self.embed_calls.push(text.to_owned());
            // Deterministic 3D hash so identical inputs yield identical
            // vectors and distinct inputs yield (mostly) distinct ones.
            let mut v = [0.0_f32; 3];
            for (i, ch) in text.chars().enumerate() {
                v[i % 3] += (ch as u32 as f32) % 7.0 + 1.0;
            }
            // Normalize so cosine similarities are well-behaved.
            let norm = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
            if norm > 0.0 {
                for x in &mut v {
                    *x /= norm;
                }
            }
            Ok(v.to_vec())
        }
    }

    /// `VectorClient` impl that always returns a fixed vector — used to
    /// assert deterministic behaviour of the cosine-search path.
    struct FixedEmbedder {
        vector: Vec<f32>,
    }

    impl VectorClient for FixedEmbedder {
        fn embed(&mut self, _text: &str) -> Result<Vec<f32>, ProjectionError> {
            Ok(self.vector.clone())
        }
    }

    /// `VectorClient` impl that returns the wrong-dimension vector — used
    /// to test the dimension-mismatch error path.
    struct WrongDimensionEmbedder;

    impl VectorClient for WrongDimensionEmbedder {
        fn embed(&mut self, _text: &str) -> Result<Vec<f32>, ProjectionError> {
            // 2D vector, but the projection is configured for 3D.
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
            },
            events,
        }
    }

    /// T1 (boundary) — `rebuild_chapter` calls `embed` once and stores
    /// the result in the in-memory map.
    #[test]
    fn test_rebuild_stores_embedding() {
        let mut proj = VectorProjection::new(MockEmbedder::default(), cfg_3d());
        let replay = make_replay("chapter-1", &[("Verified", "alpha-body")]);
        proj.rebuild_chapter(&replay)
            .expect("rebuild should succeed");
        assert_eq!(proj.embeddings().len(), 1);
        assert!(proj.embeddings().contains_key("chapter-1"));
        assert_eq!(
            proj.embeddings()["chapter-1"].len(),
            3,
            "stored vector must match configured dimension"
        );
    }

    /// T2 (property) — re-rebuilding the same chapter replaces its
    /// embedding (does not duplicate).
    #[test]
    fn test_rebuild_replaces_existing_embedding() {
        let mut proj = VectorProjection::new(MockEmbedder::default(), cfg_3d());
        let replay_v1 = make_replay("chapter-2", &[("Verified", "v1-body")]);
        let replay_v2 = make_replay("chapter-2", &[("Verified", "v2-body-updated")]);
        proj.rebuild_chapter(&replay_v1).expect("v1");
        proj.rebuild_chapter(&replay_v2).expect("v2");
        assert_eq!(proj.embeddings().len(), 1, "must not duplicate the chapter");
    }

    /// T3 (boundary) — `rebuild_chapter` returns an error when the
    /// embedder produces a wrong-dimension vector.
    #[test]
    fn test_rebuild_errors_on_dimension_mismatch() {
        let mut proj = VectorProjection::new(WrongDimensionEmbedder, cfg_3d());
        let replay = make_replay("chapter-3", &[("Verified", "body")]);
        let err = proj.rebuild_chapter(&replay).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("dimension mismatch"),
            "error must mention dimension mismatch; got: {msg}"
        );
    }

    /// T4 (property) — `render_text` skips non-`section_append` events.
    #[test]
    fn test_render_text_skips_non_section_events() {
        let chapter_id = "chapter-4";
        let mixed = ChapterReplay {
            meta: ChapterMeta {
                chapter_id: ChapterId(chapter_id.to_owned()),
                schema_id: "journal-mcp-canonical-v1".to_owned(),
                current_state: "closed".to_owned(),
                opened_at: 1_000_000_000,
                closed_at: Some(1_000_000_500),
            },
            events: vec![
                EventRow {
                    event_id: EventId("evt-open".to_owned()),
                    event_type: "open".to_owned(),
                    section_name: None,
                    payload: serde_json::json!({ "initial_state": "open" }).to_string(),
                    previous_id: None,
                    created_at: 1_000_000_000,
                },
                EventRow {
                    event_id: EventId("evt-section".to_owned()),
                    event_type: "section_append".to_owned(),
                    section_name: Some("Verified".to_owned()),
                    payload: serde_json::json!({ "body": "indexed-token" }).to_string(),
                    previous_id: None,
                    created_at: 1_000_000_100,
                },
                EventRow {
                    event_id: EventId("evt-close".to_owned()),
                    event_type: "close".to_owned(),
                    section_name: None,
                    payload: serde_json::json!({}).to_string(),
                    previous_id: None,
                    created_at: 1_000_000_500,
                },
            ],
        };
        let text = VectorProjection::<MockEmbedder>::render_text(&mixed).expect("render");
        assert_eq!(text, "indexed-token");
        assert!(!text.contains("initial_state"));
    }

    /// T5 (property) — `search` ranks an exactly-matching vector first.
    #[test]
    fn test_search_ranks_exact_match_first() {
        // Two chapters with distinct embeddings.
        let mut proj = VectorProjection::new(
            FixedEmbedder {
                vector: vec![1.0, 0.0, 0.0],
            },
            cfg_3d(),
        );
        // Both rebuilds receive the same FixedEmbedder vector, so we
        // manually inject distinct stored embeddings via the projection's
        // internal map to test the search ranking.
        proj.embeddings
            .insert("chapter-A".to_owned(), vec![1.0, 0.0, 0.0]);
        proj.embeddings
            .insert("chapter-B".to_owned(), vec![0.0, 1.0, 0.0]);
        // FixedEmbedder returns [1, 0, 0] for any query, so chapter-A
        // (which has the same vector) must rank first.
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
        let mut proj = VectorProjection::new(
            FixedEmbedder {
                vector: vec![1.0, 0.0, 0.0],
            },
            cfg_3d(),
        );
        for i in 0..5 {
            proj.embeddings
                .insert(format!("chapter-{i}"), vec![1.0, 0.0, 0.0]);
        }
        let results = proj.search("query", 2).expect("search");
        assert_eq!(results.len(), 2);
    }

    /// T7 (boundary) — `search` returns an error when the query
    /// embedding's dimension does not match the configured dimension.
    #[test]
    fn test_search_errors_on_query_dimension_mismatch() {
        let mut proj = VectorProjection::new(WrongDimensionEmbedder, cfg_3d());
        let err = proj.search("query", 1).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("dimension mismatch"),
            "error must mention dimension mismatch; got: {msg}"
        );
    }

    /// T8 (property) — `name()` returns `"vector"`.
    #[test]
    fn test_name_returns_vector() {
        let proj = VectorProjection::new(MockEmbedder::default(), cfg_3d());
        assert_eq!(proj.name(), "vector");
    }

    /// T9 (property) — `cosine_similarity` matches hand-computed values.
    #[test]
    fn test_cosine_similarity_values() {
        assert!((cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert!((cosine_similarity(&[1.0, 0.0], &[-1.0, 0.0]) - (-1.0)).abs() < 1e-6);
        // Length mismatch returns 0.0 (not NaN).
        assert_eq!(cosine_similarity(&[1.0, 0.0], &[1.0]), 0.0);
        // All-zero returns 0.0.
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
    }
}
