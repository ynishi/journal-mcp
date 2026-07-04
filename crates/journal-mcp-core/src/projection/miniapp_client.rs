//! `MiniAppCoreClient` — concrete [`MiniAppClient`] impl using
//! `mini-app-core` directly (no IPC, no MCP wire).
//!
//! # δ-2 (SDK-direct path)
//!
//! `mini-app-core` is published on crates.io as a **transport-agnostic
//! CRUD library** ("Agent-First CRUD store core library — schema.yaml
//! driven, SQLite backend"), so δ-2 imports it as a regular Cargo
//! dependency and routes [`MiniAppClient`] method calls to the
//! corresponding `Store` async APIs via in-process function calls.
//!
//! Compared to the alternative (γ-2 style `rmcp` stdio child-process
//! client):
//!
//! | Axis | SDK-direct (this module) | rmcp stdio (γ-2) |
//! |---|---|---|
//! | Latency | ~ns (function call) | ~ms (IPC roundtrip) |
//! | LOC | ~150 | ~280 |
//! | Setup | feature flag opt-in | spawn + handshake + wire |
//! | Concurrency | standard Rust | serialized stdio |
//! | Cross-language | no | yes |
//!
//! # Feature gate
//!
//! This module is gated behind the `miniapp-core` Cargo feature so
//! callers that do not need the MiniApp projection do not pay the
//! `mini-app-core` + `tokio` + `serde_yaml_bw` dependency cost.

use std::path::Path;

use mini_app_core::filter::ListFilter;
use mini_app_core::schema::SchemaConfig;
use mini_app_core::store::Store;
use mini_app_core::UpdateMode;

use super::miniapp::MiniAppClient;
use super::ProjectionError;

// ---------------------------------------------------------------------------
// Sync wrappers over tokio futures
// ---------------------------------------------------------------------------

/// Block on a tokio future from sync context using the current tokio
/// runtime.
///
/// [`MiniAppClient`] trait methods are sync; mini-app-core's `Store`
/// APIs are async.  This bridge uses [`tokio::task::block_in_place`] so
/// the projection can be driven from inside an existing tokio runtime
/// (e.g. the `journal-mcp` server binary's `#[tokio::main]` async fn).
///
/// # Caveat
///
/// `block_in_place` requires a multi-threaded tokio runtime
/// (`rt-multi-thread` flavor). Callers that use a current-thread runtime
/// must spawn the projection on a dedicated worker thread.
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(future))
}

// ---------------------------------------------------------------------------
// MiniAppCoreClient
// ---------------------------------------------------------------------------

/// Concrete [`MiniAppClient`] implementation backed by
/// [`mini_app_core::store::Store`].
///
/// Each instance owns a single `Store` (one SQLite `<table>.db` file
/// managed by mini-app-core). Constructed via
/// [`open`](MiniAppCoreClient::open), which parses the YAML schema and
/// opens the underlying SQLite handle — both operations are eager and
/// performed at construction time.
pub struct MiniAppCoreClient {
    store: Store,
}

impl MiniAppCoreClient {
    /// Open (or create) the mini-app store at `db_path`, parsing the
    /// supplied YAML schema string.
    ///
    /// `mini-app-core::Store::open` creates the underlying SQLite table
    /// on first use, so a subsequent
    /// [`schema_ensure`](MiniAppClient::schema_ensure) call from
    /// [`MiniAppProjection::rebuild_chapter`](super::MiniAppProjection::rebuild_chapter)
    /// is a no-op on this client.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectionError`] if YAML parsing or the underlying
    /// SQLite open fails.
    pub fn open(db_path: &Path, schema_yaml: &str) -> Result<Self, ProjectionError> {
        let schema: SchemaConfig = serde_yaml_bw::from_str(schema_yaml).map_err(|e| {
            ProjectionError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("mini-app-core schema YAML parse: {e}"),
            ))
        })?;
        let store = block_on(Store::open(db_path, schema)).map_err(|e| {
            ProjectionError::Io(std::io::Error::other(format!(
                "mini-app-core Store::open: {e}"
            )))
        })?;
        Ok(Self { store })
    }
}

impl MiniAppClient for MiniAppCoreClient {
    /// No-op for the SDK-direct path: `mini-app-core::Store::open`
    /// (called from [`MiniAppCoreClient::open`]) already created the
    /// table with the supplied schema. There is no separate
    /// "ensure table exists" API in mini-app-core for the single-table
    /// `Store` mode.
    fn schema_ensure(&mut self, _table: &str, _schema_yaml: &str) -> Result<(), ProjectionError> {
        Ok(())
    }

    /// Look up a row whose `chapter_id` field equals the given value.
    ///
    /// Routes to `Store::list(limit=1, offset=None, filter=Eq{...})` and
    /// returns the first row's `id` (or `None` if no row matches).
    fn row_query_by_chapter_id(
        &mut self,
        _table: &str,
        chapter_id: &str,
    ) -> Result<Option<String>, ProjectionError> {
        let filter = ListFilter::Eq {
            field: "chapter_id".to_owned(),
            value: serde_json::Value::String(chapter_id.to_owned()),
        };
        let rows = block_on(self.store.list(Some(1), None, Some(filter), None)).map_err(|e| {
            ProjectionError::Io(std::io::Error::other(format!(
                "mini-app-core Store::list: {e}"
            )))
        })?;
        Ok(rows.into_iter().next().map(|r| r.id))
    }

    /// Insert a new row with the supplied JSON data.
    ///
    /// Routes to `Store::create(value)` and returns the newly-created
    /// `row_id`.
    fn row_create(
        &mut self,
        _table: &str,
        data: &serde_json::Value,
    ) -> Result<String, ProjectionError> {
        let record = block_on(self.store.create(data.clone())).map_err(|e| {
            ProjectionError::Io(std::io::Error::other(format!(
                "mini-app-core Store::create: {e}"
            )))
        })?;
        Ok(record.id)
    }

    /// Update an existing row's JSON data with RFC 7396 merge semantics.
    ///
    /// Routes to `Store::update(row_id, value, UpdateMode::Merge)`.
    /// Merge mode is used so partial payloads are accepted; full
    /// replacement is available by switching to
    /// [`UpdateMode::Replace`] in a future enhancement.
    fn row_update(
        &mut self,
        _table: &str,
        row_id: &str,
        data: &serde_json::Value,
    ) -> Result<(), ProjectionError> {
        block_on(self.store.update(row_id, data.clone(), UpdateMode::Merge)).map_err(|e| {
            ProjectionError::Io(std::io::Error::other(format!(
                "mini-app-core Store::update: {e}"
            )))
        })?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal schema YAML that conforms to mini-app-core's `SchemaConfig`
    /// format and is sufficient for the round-trip tests below.
    const TEST_SCHEMA_YAML: &str = r#"
table: journal_chapter_test
fields:
  - name: chapter_id
    type: string
    required: true
  - name: project_label
    type: string
    required: true
"#;

    /// T1 (boundary) — round trip: open, create, query_by_chapter_id,
    /// update — verifies the full CRUD path through the real `Store`
    /// (in-process SQLite via mini-app-core).
    #[tokio::test(flavor = "multi_thread")]
    async fn test_round_trip_create_query_update() {
        let tmp = tempfile::TempDir::new().expect("TempDir should succeed");
        let db_path = tmp.path().join("journal_chapter.db");

        // Construction + CRUD operations are all sync. spawn_blocking moves
        // them onto a worker so block_in_place can find the multi-threaded
        // runtime.
        let outcome = tokio::task::spawn_blocking(move || {
            let mut client =
                MiniAppCoreClient::open(&db_path, TEST_SCHEMA_YAML).expect("open should succeed");

            let create_payload = serde_json::json!({
                "chapter_id": "chapter-alpha",
                "project_label": "test",
            });
            let row_id = client
                .row_create("journal_chapter_test", &create_payload)
                .expect("row_create should succeed");
            assert!(
                !row_id.is_empty(),
                "row_create must return a non-empty row_id"
            );

            let queried = client
                .row_query_by_chapter_id("journal_chapter_test", "chapter-alpha")
                .expect("query should succeed");
            assert_eq!(
                queried.as_deref(),
                Some(row_id.as_str()),
                "query must return the same row_id"
            );

            let update_payload = serde_json::json!({
                "chapter_id": "chapter-alpha",
                "project_label": "test-updated",
            });
            client
                .row_update("journal_chapter_test", &row_id, &update_payload)
                .expect("row_update should succeed");
        })
        .await;
        outcome.expect("spawn_blocking should succeed");
    }

    /// T2 (boundary) — `row_query_by_chapter_id` returns `Ok(None)` when
    /// no row matches.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_query_returns_none_when_absent() {
        let tmp = tempfile::TempDir::new().expect("TempDir should succeed");
        let db_path = tmp.path().join("journal_chapter.db");

        let queried = tokio::task::spawn_blocking(move || {
            let mut client =
                MiniAppCoreClient::open(&db_path, TEST_SCHEMA_YAML).expect("open should succeed");
            client
                .row_query_by_chapter_id("journal_chapter_test", "does-not-exist")
                .expect("query should succeed")
        })
        .await
        .expect("spawn_blocking should succeed");

        assert!(queried.is_none(), "absent chapter must return None");
    }

    /// T3 (property) — `schema_ensure` is a no-op (always succeeds, does
    /// not panic, does not touch the underlying SQLite).
    #[tokio::test(flavor = "multi_thread")]
    async fn test_schema_ensure_is_noop() {
        let tmp = tempfile::TempDir::new().expect("TempDir should succeed");
        let db_path = tmp.path().join("journal_chapter.db");

        tokio::task::spawn_blocking(move || {
            let mut client =
                MiniAppCoreClient::open(&db_path, TEST_SCHEMA_YAML).expect("open should succeed");
            client
                .schema_ensure("journal_chapter_test", TEST_SCHEMA_YAML)
                .expect("schema_ensure must always succeed");
        })
        .await
        .expect("spawn_blocking should succeed");
    }

    /// T4 (boundary) — `open` returns an error when the YAML schema is
    /// malformed (invalid YAML).
    #[tokio::test(flavor = "multi_thread")]
    async fn test_open_returns_err_on_bad_yaml() {
        let tmp = tempfile::TempDir::new().expect("TempDir should succeed");
        let db_path = tmp.path().join("journal_chapter.db");
        let bad_yaml = "fields: [not-a-valid-shape-for-mini-app-core";

        let result =
            tokio::task::spawn_blocking(move || MiniAppCoreClient::open(&db_path, bad_yaml))
                .await
                .expect("spawn_blocking should succeed");

        assert!(
            result.is_err(),
            "malformed YAML must produce a ProjectionError"
        );
    }
}
