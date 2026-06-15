//! `OutlineProjection` — sync chapters as nodes in an Outline-MCP book.
//!
//! Projects each chapter to a node in a configured Outline-MCP book,
//! enabling cross-project chapter search via the Outline knowledge graph
//! and embedding journal content into broader knowledge trees (rules /
//! runbooks / decision logs).
//!
//! # Design split (γ-1 / γ-2)
//!
//! This module implements **γ-1**: the projection logic + the
//! [`OutlineClient`] trait abstraction over the Outline-MCP wire surface.
//! All chapter-write operations route through the trait, which is
//! mock-implementable for testing.
//!
//! The concrete `rmcp` child-process client (`RmcpStdioOutlineClient`)
//! that spawns the real `outline-mcp` binary and talks to it over stdio
//! is **γ-2** and lands in a follow-up commit on the same topic branch.
//! That split keeps γ-1 self-contained and unit-testable without
//! requiring a running `outline-mcp` process.
//!
//! # Node mapping
//!
//! ```text
//! Outline book = config.book_slug
//!   └── parent node = config.parent_node_path (default: "Chapters")
//!         ├── node[chapter_id-1]  ← one node per chapter
//!         ├── node[chapter_id-2]
//!         └── …
//! ```
//!
//! Each chapter's node body is the rendered Markdown of its sections
//! (heading + 5 required sections + any optional sections present in
//! the chapter).  Re-rebuilding an existing chapter updates the node
//! body in place.

use std::collections::HashSet;

use super::{private::Sealed, JournalProjection, ProjectionError};
use crate::{ChapterId, ChapterReplay};

// ---------------------------------------------------------------------------
// OutlineClient trait — wire-format abstraction
// ---------------------------------------------------------------------------

/// Minimal wire surface that [`OutlineProjection`] requires from an
/// Outline-MCP client.
///
/// Implementations are crate-internal (sealed via `pub(crate) Sealed` in
/// future, currently `pub` for the γ-2 wiring commit) and route to either
/// the real `rmcp` child-process Outline-MCP server or a test mock.
///
/// All three operations are idempotent at the projection level:
/// [`OutlineProjection::rebuild_chapter`] always calls `node_query` first
/// and routes to `node_update` when a node exists or `node_create` when
/// it does not — so the same chapter can be rebuilt repeatedly without
/// duplicating nodes.
pub trait OutlineClient: Send {
    /// Look up a node by slug under a given parent node.
    ///
    /// Returns `Ok(Some(node_id))` if the node exists, `Ok(None)` if no
    /// node with the given slug is present, or `Err(...)` on transport
    /// failure.
    fn node_query(
        &mut self,
        book_slug: &str,
        parent_node_path: &str,
        node_slug: &str,
    ) -> Result<Option<String>, ProjectionError>;

    /// Create a new node under the given parent.
    ///
    /// Returns the newly-created `node_id` on success.
    fn node_create(
        &mut self,
        book_slug: &str,
        parent_node_path: &str,
        node_slug: &str,
        body: &str,
    ) -> Result<String, ProjectionError>;

    /// Update the body of an existing node.
    fn node_update(
        &mut self,
        book_slug: &str,
        node_id: &str,
        body: &str,
    ) -> Result<(), ProjectionError>;
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for [`OutlineProjection`].
#[derive(Debug, Clone)]
pub struct OutlineConfig {
    /// Target Outline book slug (e.g. `"journal-myproject"`).
    pub book_slug: String,
    /// Parent node path under which chapter nodes are created
    /// (e.g. `"Chapters"`).  Hierarchical nesting is supported via slash
    /// separators in the v0.3.0 design's flat default; deeper hierarchies
    /// are a follow-up enhancement (design doc §7 open question #2).
    pub parent_node_path: String,
}

impl Default for OutlineConfig {
    fn default() -> Self {
        Self {
            book_slug: "journal".to_owned(),
            parent_node_path: "Chapters".to_owned(),
        }
    }
}

// ---------------------------------------------------------------------------
// OutlineProjection
// ---------------------------------------------------------------------------

/// Outline-MCP sync projection, generic over the [`OutlineClient`]
/// implementation.
///
/// See the module-level documentation for the design overview and the
/// γ-1 / γ-2 split.
pub struct OutlineProjection<C: OutlineClient> {
    /// Client that talks to the Outline-MCP backend (real `rmcp` stdio
    /// child or test mock).
    client: C,

    /// Static configuration (book slug + parent node path).
    config: OutlineConfig,

    /// Chapter IDs queued for rebuild on the next batch flush.
    ///
    /// `mark_dirty` inserts; `rebuild_chapter` removes after a successful
    /// node write.  Used by callers that want to batch-flush multiple
    /// dirty chapters before invoking `rebuild_chapter` for each.
    dirty: HashSet<String>,
}

impl<C: OutlineClient> OutlineProjection<C> {
    /// Construct an `OutlineProjection` with the given client and config.
    pub fn new(client: C, config: OutlineConfig) -> Self {
        Self {
            client,
            config,
            dirty: HashSet::new(),
        }
    }

    /// Return a reference to the set of dirty chapter IDs.
    ///
    /// Used by tests to inspect internal state.
    pub fn dirty_chapters(&self) -> &HashSet<String> {
        &self.dirty
    }

    /// Render a chapter to a Markdown body suitable for an Outline node.
    ///
    /// Format: chapter heading as an H1, then each `section_append` event
    /// as an H2 section.  Non-`section_append` events (open / close /
    /// append_progress / import) are skipped — they do not contribute to
    /// the human-readable body.
    fn render_body(replay: &ChapterReplay) -> Result<String, ProjectionError> {
        let mut out = String::new();
        out.push_str("# ");
        out.push_str(&replay.meta.chapter_id.0);
        out.push_str("\n\n");
        for event in &replay.events {
            if event.event_type != "section_append" {
                continue;
            }
            let section_name = event.section_name.as_deref().unwrap_or("");
            let payload: serde_json::Value = serde_json::from_str(&event.payload)?;
            let body = payload
                .get("body")
                .and_then(|b| b.as_str())
                .unwrap_or_default();
            out.push_str("## ");
            out.push_str(section_name);
            out.push_str("\n\n");
            out.push_str(body.trim());
            out.push_str("\n\n");
        }
        Ok(out)
    }
}

impl<C: OutlineClient> Sealed for OutlineProjection<C> {}

impl<C: OutlineClient + 'static> JournalProjection for OutlineProjection<C> {
    fn name(&self) -> &'static str {
        "outline"
    }

    fn mark_dirty(&mut self, id: &ChapterId) -> Result<(), ProjectionError> {
        self.dirty.insert(id.0.clone());
        Ok(())
    }

    /// Sync the chapter to its corresponding Outline node.
    ///
    /// Pipeline:
    /// 1. Render the chapter body to Markdown.
    /// 2. Query the Outline backend for an existing node keyed by
    ///    `chapter_id` slug under the configured parent.
    /// 3. If the node exists, call `node_update` with the new body.
    ///    Otherwise call `node_create` to insert a fresh node.
    /// 4. Remove the chapter from the dirty set on success.
    fn rebuild_chapter(&mut self, replay: &ChapterReplay) -> Result<(), ProjectionError> {
        let chapter_id = replay.meta.chapter_id.0.clone();
        let body = Self::render_body(replay)?;

        let existing = self.client.node_query(
            &self.config.book_slug,
            &self.config.parent_node_path,
            &chapter_id,
        )?;

        match existing {
            Some(node_id) => {
                self.client
                    .node_update(&self.config.book_slug, &node_id, &body)?;
            }
            None => {
                self.client.node_create(
                    &self.config.book_slug,
                    &self.config.parent_node_path,
                    &chapter_id,
                    &body,
                )?;
            }
        }

        self.dirty.remove(&chapter_id);
        Ok(())
    }
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
    struct MockOutlineClient {
        /// Pre-populated map: (book_slug, parent_path, node_slug) → node_id.
        existing: std::collections::HashMap<(String, String, String), String>,
        created: Vec<(String, String, String, String)>,
        updated: Vec<(String, String, String)>,
        queried: Vec<(String, String, String)>,
    }

    impl MockOutlineClient {
        fn with_existing(mut self, book: &str, parent: &str, slug: &str, node_id: &str) -> Self {
            self.existing.insert(
                (book.to_owned(), parent.to_owned(), slug.to_owned()),
                node_id.to_owned(),
            );
            self
        }
    }

    impl OutlineClient for MockOutlineClient {
        fn node_query(
            &mut self,
            book_slug: &str,
            parent_node_path: &str,
            node_slug: &str,
        ) -> Result<Option<String>, ProjectionError> {
            self.queried.push((
                book_slug.to_owned(),
                parent_node_path.to_owned(),
                node_slug.to_owned(),
            ));
            Ok(self
                .existing
                .get(&(
                    book_slug.to_owned(),
                    parent_node_path.to_owned(),
                    node_slug.to_owned(),
                ))
                .cloned())
        }

        fn node_create(
            &mut self,
            book_slug: &str,
            parent_node_path: &str,
            node_slug: &str,
            body: &str,
        ) -> Result<String, ProjectionError> {
            let node_id = format!("created-{}", self.created.len());
            self.created.push((
                book_slug.to_owned(),
                parent_node_path.to_owned(),
                node_slug.to_owned(),
                body.to_owned(),
            ));
            self.existing.insert(
                (
                    book_slug.to_owned(),
                    parent_node_path.to_owned(),
                    node_slug.to_owned(),
                ),
                node_id.clone(),
            );
            Ok(node_id)
        }

        fn node_update(
            &mut self,
            book_slug: &str,
            node_id: &str,
            body: &str,
        ) -> Result<(), ProjectionError> {
            self.updated
                .push((book_slug.to_owned(), node_id.to_owned(), body.to_owned()));
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

    /// T1 (boundary) — fresh chapter triggers `node_create` (existing node
    /// is None).
    #[test]
    fn test_rebuild_calls_node_create_for_fresh_chapter() {
        let client = MockOutlineClient::default();
        let mut proj = OutlineProjection::new(client, OutlineConfig::default());
        let replay = make_replay("chapter-1", &[("Verified", "alpha-body")]);

        proj.rebuild_chapter(&replay).expect("rebuild");
        assert_eq!(proj.client.queried.len(), 1, "must query first");
        assert_eq!(proj.client.created.len(), 1, "must create when None");
        assert_eq!(proj.client.updated.len(), 0, "must not update when None");
        let (book, parent, slug, body) = &proj.client.created[0];
        assert_eq!(book, "journal");
        assert_eq!(parent, "Chapters");
        assert_eq!(slug, "chapter-1");
        assert!(body.contains("# chapter-1"));
        assert!(body.contains("## Verified"));
        assert!(body.contains("alpha-body"));
    }

    /// T2 (boundary) — existing chapter triggers `node_update`.
    #[test]
    fn test_rebuild_calls_node_update_for_existing_chapter() {
        let client = MockOutlineClient::default().with_existing(
            "journal",
            "Chapters",
            "chapter-2",
            "node-abc",
        );
        let mut proj = OutlineProjection::new(client, OutlineConfig::default());
        let replay = make_replay("chapter-2", &[("Verified", "beta-body")]);

        proj.rebuild_chapter(&replay).expect("rebuild");
        assert_eq!(proj.client.queried.len(), 1);
        assert_eq!(proj.client.created.len(), 0, "must not create when Some");
        assert_eq!(proj.client.updated.len(), 1, "must update when Some");
        let (book, node_id, body) = &proj.client.updated[0];
        assert_eq!(book, "journal");
        assert_eq!(node_id, "node-abc");
        assert!(body.contains("beta-body"));
    }

    /// T3 (property) — `mark_dirty` queues the chapter; `rebuild_chapter`
    /// removes it from the queue.
    #[test]
    fn test_mark_dirty_then_rebuild_clears_dirty() {
        let client = MockOutlineClient::default();
        let mut proj = OutlineProjection::new(client, OutlineConfig::default());
        let id = ChapterId("chapter-3".to_owned());
        proj.mark_dirty(&id).expect("mark");
        assert!(proj.dirty_chapters().contains("chapter-3"));

        let replay = make_replay("chapter-3", &[("Verified", "gamma")]);
        proj.rebuild_chapter(&replay).expect("rebuild");
        assert!(
            !proj.dirty_chapters().contains("chapter-3"),
            "rebuild must clear the dirty entry"
        );
    }

    /// T4 (property) — rebuilding the same chapter twice is idempotent at
    /// the wire layer: the second call routes to `node_update`, not
    /// `node_create` (because the first call inserted the slug into the
    /// mock's existing-map).
    #[test]
    fn test_rebuild_idempotent_routes_second_call_to_update() {
        let client = MockOutlineClient::default();
        let mut proj = OutlineProjection::new(client, OutlineConfig::default());
        let replay = make_replay("chapter-4", &[("Verified", "delta")]);

        proj.rebuild_chapter(&replay).expect("first");
        proj.rebuild_chapter(&replay).expect("second");
        assert_eq!(
            proj.client.created.len(),
            1,
            "exactly one node_create across two rebuilds"
        );
        assert_eq!(
            proj.client.updated.len(),
            1,
            "second rebuild must route to node_update"
        );
    }

    /// T5 (property) — `render_body` skips non-`section_append` events
    /// (open / close / append_progress / import).
    #[test]
    fn test_render_body_skips_non_section_events() {
        let chapter_id = "chapter-5";
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
                    payload: serde_json::json!({ "body": "epsilon-section" }).to_string(),
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

        let body = OutlineProjection::<MockOutlineClient>::render_body(&mixed_replay)
            .expect("render should succeed");
        assert!(body.contains("# chapter-5"));
        assert!(body.contains("## Verified"));
        assert!(body.contains("epsilon-section"));
        assert!(
            !body.contains("initial_state"),
            "open-event payload must not leak into the rendered body"
        );
    }

    /// T6 (boundary) — config is forwarded verbatim to the client (custom
    /// book_slug + parent_node_path).
    #[test]
    fn test_config_forwarded_to_client() {
        let client = MockOutlineClient::default();
        let config = OutlineConfig {
            book_slug: "my-custom-book".to_owned(),
            parent_node_path: "Logs/2026".to_owned(),
        };
        let mut proj = OutlineProjection::new(client, config);
        let replay = make_replay("chapter-6", &[("Verified", "zeta")]);
        proj.rebuild_chapter(&replay).expect("rebuild");

        let (book, parent, slug, _body) = &proj.client.created[0];
        assert_eq!(book, "my-custom-book");
        assert_eq!(parent, "Logs/2026");
        assert_eq!(slug, "chapter-6");
    }

    /// T7 (property) — `name()` returns `"outline"` (stable identifier
    /// used by `JournalCore::rebuild_projection` / `list_projection_names`).
    #[test]
    fn test_name_returns_outline() {
        let proj = OutlineProjection::new(MockOutlineClient::default(), OutlineConfig::default());
        assert_eq!(proj.name(), "outline");
    }
}
