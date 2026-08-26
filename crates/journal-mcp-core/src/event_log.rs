//! EventLog — append-only SQLite-backed canonical chapter history.
//!
//! Implements the schema from `docs/design.md §8.1` with PRAGMA settings from `§9.3`.
//! The `event_log` table is immutable at the database level via BEFORE UPDATE/BEFORE DELETE
//! triggers. `chapter_meta` has no triggers so state-transition UPDATEs succeed.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension};
use ulid::Ulid;

// ─────────────────────────── Error type ────────────────────────────────────

/// Errors that can be returned by [`EventLog`] operations.
#[derive(Debug, thiserror::Error)]
pub enum EventLogError {
    /// A rusqlite / SQLite operation failed.
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// System time was before the Unix epoch.
    #[error("system time error: {0}")]
    Time(#[from] std::time::SystemTimeError),

    /// A serde_json (de)serialisation operation failed.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// The requested chapter does not exist in `chapter_meta`.
    #[error("chapter not found: {0}")]
    ChapterNotFound(String),

    /// A state transition was attempted from an incompatible current state.
    #[error("invalid state transition: current={current}, attempted={attempted}")]
    InvalidState {
        /// The current state recorded in `chapter_meta`.
        current: String,
        /// The state that was attempted.
        attempted: String,
    },

    /// A chapter with this id already exists.
    ///
    /// Distinguished from a generic SQL failure so callers see a domain
    /// error instead of a raw `UNIQUE constraint failed` string.
    #[error("chapter already exists: {0}")]
    ChapterAlreadyExists(String),

    /// An events-import row carries an `event_id` that already exists in this
    /// store with **different** content.
    ///
    /// Identical rows are skipped silently (idempotent re-import); only a
    /// same-id / different-content pair is an integrity violation, and the
    /// whole import transaction is rolled back when it is detected.
    #[error("event conflict: event_id={event_id} already exists with different content")]
    EventConflict {
        /// The colliding event identifier.
        event_id: String,
    },
}

// ─────────────────────────── Newtype wrappers ───────────────────────────────

/// Opaque identifier for a chapter (date-slug or similar).
///
/// The inner `String` maps to the `stream_id` column in `event_log` and to
/// `chapter_id` in `chapter_meta`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChapterId(pub String);

impl std::fmt::Display for ChapterId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Opaque identifier for a single event row (ULID string).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventId(pub String);

impl std::fmt::Display for EventId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

// ─────────────────────────── Public data structs ────────────────────────────

/// Metadata row for a chapter stored in `chapter_meta`.
#[derive(Debug, Clone)]
pub struct ChapterMeta {
    /// Chapter identifier.
    pub chapter_id: ChapterId,
    /// Schema identifier used to interpret the chapter's events.
    pub schema_id: String,
    /// Current state of the chapter's state machine (e.g. `"open"`, `"appending"`, `"closed"`).
    pub current_state: String,
    /// Unix epoch milliseconds when the chapter was opened.
    pub opened_at: i64,
    /// Unix epoch milliseconds when the chapter was closed, or `None` if still open.
    pub closed_at: Option<i64>,
    /// Human-written chapter name, if the store recorded one. `chapter_id` is
    /// its slug, so a header's `{name}` slot falls back to the slug when this
    /// is `None` (rows written before the column existed).
    pub chapter_name: Option<String>,
}

/// A single row from the `event_log` table.
#[derive(Debug, Clone)]
pub struct EventRow {
    /// Unique event identifier (ULID).
    pub event_id: EventId,
    /// Event type string, e.g. `"open"`, `"section_append"`, `"close"`.
    pub event_type: String,
    /// Section name, present only for `section_append` events.
    pub section_name: Option<String>,
    /// JSON payload for the event.
    pub payload: String,
    /// Previous event in a correction chain, or `None`.
    pub previous_id: Option<EventId>,
    /// Unix epoch milliseconds when the event was created.
    pub created_at: i64,
}

/// A full chapter replay: metadata plus all events in append order.
#[derive(Debug, Clone)]
pub struct ChapterReplay {
    /// Chapter metadata from `chapter_meta`.
    pub meta: ChapterMeta,
    /// Events ordered by `event_id` (ULID lexicographic = time order).
    pub events: Vec<EventRow>,
}

/// A verbatim `event_log` row (including `stream_id`), used by the
/// events-native export/import path.
///
/// Unlike [`EventRow`] (a per-chapter replay view that omits `stream_id`),
/// this struct mirrors the physical table column-for-column so a row can be
/// transferred to another store and inserted without loss.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawEventRow {
    /// Unique event identifier (ULID) — the idempotency/dedup key.
    pub event_id: String,
    /// Stream the event belongs to (chapter_id, or migration_id for `import` events).
    pub stream_id: String,
    /// Event type string, e.g. `"open"`, `"section_append"`, `"close"`, `"import"`.
    pub event_type: String,
    /// Section name, present only for `section_append` events.
    pub section_name: Option<String>,
    /// JSON payload for the event.
    pub payload: String,
    /// Previous event in a correction chain, or `None`.
    pub previous_id: Option<String>,
    /// Unix epoch milliseconds when the event was created (preserved on transfer).
    pub created_at: i64,
}

// ─────────────────────────── DDL + PRAGMA constants ─────────────────────────

const PRAGMA_WAL: &str = "PRAGMA journal_mode=WAL";
const PRAGMA_SYNC: &str = "PRAGMA synchronous=NORMAL";
const PRAGMA_FK: &str = "PRAGMA foreign_keys=ON";

const DDL_EVENT_LOG: &str = "
CREATE TABLE IF NOT EXISTS event_log (
    event_id     TEXT PRIMARY KEY,
    stream_id    TEXT NOT NULL,
    event_type   TEXT NOT NULL,
    section_name TEXT,
    payload      TEXT NOT NULL,
    previous_id  TEXT,
    created_at   INTEGER NOT NULL
) STRICT";

const DDL_CHAPTER_META: &str = "
CREATE TABLE IF NOT EXISTS chapter_meta (
    chapter_id    TEXT PRIMARY KEY,
    schema_id     TEXT NOT NULL,
    current_state TEXT NOT NULL,
    opened_at     INTEGER NOT NULL,
    closed_at     INTEGER,
    chapter_name  TEXT
) STRICT";

/// Additive migration for stores created before `chapter_name` existed.
///
/// The column holds the chapter's human name as written (`chapter_id` is its
/// slug). Without it the projection had nothing to put in a header's `{name}`
/// slot and rendered the slug twice, losing the original title. Older rows
/// keep `NULL` and fall back to the slug, exactly as before.
const MIGRATION_CHAPTER_NAME: &str = "ALTER TABLE chapter_meta ADD COLUMN chapter_name TEXT";

/// BEFORE UPDATE trigger on `event_log` — raises ABORT to enforce append-only immutability.
///
/// This is a Crux must_not_simplify constraint: the database-level trigger must exist;
/// relying solely on Rust API surface is insufficient.
const TRIGGER_NO_UPDATE: &str = "
CREATE TRIGGER IF NOT EXISTS event_log_no_update BEFORE UPDATE ON event_log
    BEGIN SELECT RAISE(ABORT, 'event_log is append-only'); END";

/// BEFORE DELETE trigger on `event_log` — raises ABORT to enforce append-only immutability.
///
/// Only `event_log` has this trigger. `chapter_meta` intentionally has no triggers
/// so that state-transition UPDATEs on that table succeed on the same connection.
const TRIGGER_NO_DELETE: &str = "
CREATE TRIGGER IF NOT EXISTS event_log_no_delete BEFORE DELETE ON event_log
    BEGIN SELECT RAISE(ABORT, 'event_log is append-only'); END";

// ─────────────────────────── EventLog ───────────────────────────────────────

/// Append-only SQLite event store for chapter history.
///
/// Opens (or creates) a database at the given path, applies WAL/synchronous/foreign_keys
/// PRAGMAs, and creates `event_log`, `chapter_meta`, and immutability triggers if they
/// do not already exist.
pub struct EventLog {
    conn: Connection,
}

impl EventLog {
    /// Open (or create) an [`EventLog`] database at `path`.
    ///
    /// Applies `PRAGMA journal_mode=WAL`, `synchronous=NORMAL`, and `foreign_keys=ON`
    /// before creating the schema. Idempotent: safe to call on an existing database.
    ///
    /// # Errors
    ///
    /// Returns [`EventLogError::Sqlite`] if the file cannot be opened, a PRAGMA fails,
    /// or DDL execution fails.
    pub fn open(path: &Path) -> Result<Self, EventLogError> {
        let conn = Connection::open(path).map_err(|e| {
            tracing::warn!(error = ?e, path = ?path, "EventLog::open failed to open connection");
            e
        })?;

        // Apply PRAGMAs before DDL (BP-3.4).
        conn.execute_batch(PRAGMA_WAL).map_err(|e| {
            tracing::warn!(error = ?e, "EventLog::open failed to set journal_mode=WAL");
            e
        })?;
        conn.execute_batch(PRAGMA_SYNC).map_err(|e| {
            tracing::warn!(error = ?e, "EventLog::open failed to set synchronous=NORMAL");
            e
        })?;
        conn.execute_batch(PRAGMA_FK).map_err(|e| {
            tracing::warn!(error = ?e, "EventLog::open failed to set foreign_keys=ON");
            e
        })?;

        // Create tables and triggers (IF NOT EXISTS → idempotent on re-open).
        conn.execute_batch(DDL_EVENT_LOG).map_err(|e| {
            tracing::warn!(error = ?e, "EventLog::open failed to create event_log table");
            e
        })?;
        conn.execute_batch(DDL_CHAPTER_META).map_err(|e| {
            tracing::warn!(error = ?e, "EventLog::open failed to create chapter_meta table");
            e
        })?;
        // Idempotent: on a store that already has the column SQLite reports a
        // duplicate-column error, which is the success case for a re-open.
        if let Err(e) = conn.execute_batch(MIGRATION_CHAPTER_NAME) {
            let msg = e.to_string();
            if !msg.contains("duplicate column name") {
                tracing::warn!(error = ?e, "EventLog::open failed to add chapter_meta.chapter_name");
                return Err(e.into());
            }
        }

        // Crux must_not_simplify: BEFORE UPDATE/BEFORE DELETE triggers on event_log only.
        // chapter_meta must NOT have these triggers (state-transition UPDATEs must succeed).
        conn.execute_batch(TRIGGER_NO_UPDATE).map_err(|e| {
            tracing::warn!(error = ?e, "EventLog::open failed to create event_log_no_update trigger");
            e
        })?;
        conn.execute_batch(TRIGGER_NO_DELETE).map_err(|e| {
            tracing::warn!(error = ?e, "EventLog::open failed to create event_log_no_delete trigger");
            e
        })?;

        Ok(Self { conn })
    }

    // ──────────────────────── helpers ────────────────────────────────────

    /// Returns the current unix epoch in milliseconds.
    fn now_ms() -> Result<i64, EventLogError> {
        Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as i64)
    }

    /// Generates a new ULID string for use as an event ID.
    fn new_event_id() -> String {
        Ulid::new().to_string()
    }

    // ──────────────────────── append API ─────────────────────────────────

    /// Open a new chapter: inserts an `open` event into `event_log` and a row into `chapter_meta`.
    ///
    /// # Arguments
    ///
    /// * `chapter_id` — date-slug or other chapter identifier (stored as `stream_id` in SQL).
    /// * `chapter_name` — the name as written; `chapter_id` is its slug.
    /// * `schema_id` — schema identifier used to interpret this chapter's events.
    /// * `initial_state` — initial state for the chapter's state machine.
    ///
    /// Both inserts run in **one transaction**: the `open` event and the
    /// `chapter_meta` row appear together or not at all. Writing the event
    /// first and the row second (as this used to) left an orphan `open` event
    /// behind whenever the row insert failed — a failed open still polluted
    /// the chapter's replay.
    ///
    /// # Errors
    ///
    /// [`EventLogError::ChapterAlreadyExists`] when `chapter_id` is taken,
    /// [`EventLogError::Sqlite`] on other SQL failure, [`EventLogError::Time`]
    /// if the system clock is before the Unix epoch.
    pub fn append_chapter_open(
        &mut self,
        chapter_id: &str,
        chapter_name: &str,
        schema_id: &str,
        initial_state: &str,
    ) -> Result<EventId, EventLogError> {
        let event_id = Self::new_event_id();
        let now = Self::now_ms()?;
        let payload = serde_json::json!({ "initial_state": initial_state }).to_string();

        let tx = self.conn.transaction().map_err(|e| {
            tracing::warn!(error = ?e, chapter_id, "append_chapter_open: begin transaction failed");
            e
        })?;

        tx.execute(
            "INSERT INTO event_log
                 (event_id, stream_id, event_type, section_name, payload, previous_id, created_at)
             VALUES (?1, ?2, 'open', NULL, ?3, NULL, ?4)",
            rusqlite::params![event_id, chapter_id, payload, now],
        )
        .map_err(|e| {
            tracing::warn!(error = ?e, chapter_id, "append_chapter_open: INSERT event_log failed");
            e
        })?;

        tx.execute(
            "INSERT INTO chapter_meta
                 (chapter_id, schema_id, current_state, opened_at, closed_at, chapter_name)
             VALUES (?1, ?2, ?3, ?4, NULL, ?5)",
            rusqlite::params![chapter_id, schema_id, initial_state, now, chapter_name],
        )
        .map_err(|e| {
            tracing::warn!(error = ?e, chapter_id, "append_chapter_open: INSERT chapter_meta failed");
            // A taken chapter_id is an ordinary caller mistake, not an
            // internal fault: report it as such instead of leaking the raw
            // `UNIQUE constraint failed: chapter_meta.chapter_id`.
            if matches!(
                e.sqlite_error_code(),
                Some(rusqlite::ErrorCode::ConstraintViolation)
            ) {
                EventLogError::ChapterAlreadyExists(chapter_id.to_owned())
            } else {
                EventLogError::Sqlite(e)
            }
        })?;

        tx.commit().map_err(|e| {
            tracing::warn!(error = ?e, chapter_id, "append_chapter_open: commit failed");
            e
        })?;

        Ok(EventId(event_id))
    }

    /// Append a section event to an existing chapter.
    ///
    /// Inserts a `section_append` event into `event_log` and updates
    /// `chapter_meta.current_state` to `next_state`.
    ///
    /// # Arguments
    ///
    /// * `chapter_id` — target chapter.
    /// * `section_name` — name of the section being appended.
    /// * `body` — section body text.
    /// * `next_state` — new state for the chapter after this append.
    /// * `previous_id` — optional previous event in a correction chain.
    ///
    /// # Errors
    ///
    /// Returns [`EventLogError::Sqlite`], [`EventLogError::Time`], or [`EventLogError::Json`]
    /// on the respective failures.
    pub fn append_section(
        &mut self,
        chapter_id: &ChapterId,
        section_name: &str,
        body: &str,
        next_state: &str,
        previous_id: Option<&EventId>,
    ) -> Result<EventId, EventLogError> {
        let event_id = Self::new_event_id();
        let now = Self::now_ms()?;
        let payload = serde_json::json!({ "body": body }).to_string();
        let prev = previous_id.map(|e| e.0.as_str());

        self.conn
            .execute(
                "INSERT INTO event_log
                 (event_id, stream_id, event_type, section_name, payload, previous_id, created_at)
             VALUES (?1, ?2, 'section_append', ?3, ?4, ?5, ?6)",
                rusqlite::params![event_id, chapter_id.0, section_name, payload, prev, now],
            )
            .map_err(|e| {
                tracing::warn!(error = ?e, chapter_id = %chapter_id, section_name, "append_section: INSERT event_log failed");
                e
            })?;

        self.conn
            .execute(
                "UPDATE chapter_meta SET current_state = ?1 WHERE chapter_id = ?2",
                rusqlite::params![next_state, chapter_id.0],
            )
            .map_err(|e| {
                tracing::warn!(error = ?e, chapter_id = %chapter_id, "append_section: UPDATE chapter_meta failed");
                e
            })?;

        Ok(EventId(event_id))
    }

    /// Close a chapter: inserts a `close` event and updates `chapter_meta` to the closed state.
    ///
    /// Sets `chapter_meta.current_state` to `next_state` and records `closed_at`.
    ///
    /// # Arguments
    ///
    /// * `chapter_id` — target chapter.
    /// * `next_state` — final state for the chapter (e.g. `"closed"`).
    ///
    /// # Errors
    ///
    /// Returns [`EventLogError::Sqlite`], [`EventLogError::Time`], or [`EventLogError::Json`]
    /// on the respective failures.
    pub fn append_close(
        &mut self,
        chapter_id: &ChapterId,
        next_state: &str,
    ) -> Result<EventId, EventLogError> {
        let event_id = Self::new_event_id();
        let now = Self::now_ms()?;
        let payload = serde_json::json!({}).to_string();

        self.conn
            .execute(
                "INSERT INTO event_log
                 (event_id, stream_id, event_type, section_name, payload, previous_id, created_at)
             VALUES (?1, ?2, 'close', NULL, ?3, NULL, ?4)",
                rusqlite::params![event_id, chapter_id.0, payload, now],
            )
            .map_err(|e| {
                tracing::warn!(error = ?e, chapter_id = %chapter_id, "append_close: INSERT event_log failed");
                e
            })?;

        self.conn
            .execute(
                "UPDATE chapter_meta SET current_state = ?1, closed_at = ?2 WHERE chapter_id = ?3",
                rusqlite::params![next_state, now, chapter_id.0],
            )
            .map_err(|e| {
                tracing::warn!(error = ?e, chapter_id = %chapter_id, "append_close: UPDATE chapter_meta failed");
                e
            })?;

        Ok(EventId(event_id))
    }

    // ──────────────────────── read API ───────────────────────────────────

    /// Replay all events for a chapter in append order.
    ///
    /// Returns a [`ChapterReplay`] containing the chapter metadata and all events
    /// ordered by `event_id` (ULID lexicographic order equals time order).
    ///
    /// For chapters that were ingested via `import_chapter`, the `event_log` table
    /// stores a single `import` event keyed by `migration_id` rather than
    /// `chapter_id`.  This method detects that case (zero direct events for the
    /// `chapter_id` stream) and expands the relevant import event into virtual
    /// synthetic `open` / `section_append` / `close` rows so that callers see a
    /// uniform `ChapterReplay` regardless of how the chapter was created.
    ///
    /// # Errors
    ///
    /// Returns [`EventLogError::ChapterNotFound`] if no `chapter_meta` row exists for
    /// `chapter_id`, or [`EventLogError::Sqlite`] on SQL failure.
    pub fn chapter(&self, chapter_id: &ChapterId) -> Result<ChapterReplay, EventLogError> {
        // Fetch chapter_meta row.
        let meta = self
            .conn
            .query_row(
                "SELECT chapter_id, schema_id, current_state, opened_at, closed_at, chapter_name
                   FROM chapter_meta
                  WHERE chapter_id = ?1",
                rusqlite::params![chapter_id.0],
                |row| {
                    Ok(ChapterMeta {
                        chapter_id: ChapterId(row.get(0)?),
                        schema_id: row.get(1)?,
                        current_state: row.get(2)?,
                        opened_at: row.get(3)?,
                        closed_at: row.get(4)?,
                        chapter_name: row.get(5)?,
                    })
                },
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    tracing::warn!(chapter_id = %chapter_id, "chapter: chapter_meta row not found");
                    EventLogError::ChapterNotFound(chapter_id.0.clone())
                }
                other => {
                    tracing::warn!(error = ?other, chapter_id = %chapter_id, "chapter: query_row chapter_meta failed");
                    EventLogError::Sqlite(other)
                }
            })?;

        // Fetch events ordered by event_id (ULID lexicographic = time order).
        // Columns are listed explicitly; indices are fixed (K-2 st2-pin).
        // 0=event_id 1=event_type 2=section_name 3=payload 4=previous_id 5=created_at
        let mut stmt = self.conn.prepare(
            "SELECT event_id, event_type, section_name, payload, previous_id, created_at
               FROM event_log
              WHERE stream_id = ?1
              ORDER BY event_id",
        ).map_err(|e| {
            tracing::warn!(error = ?e, chapter_id = %chapter_id, "chapter: prepare event_log SELECT failed");
            e
        })?;

        let events = stmt
            .query_map(rusqlite::params![chapter_id.0], |row| {
                let prev_str: Option<String> = row.get(4)?;
                Ok(EventRow {
                    event_id: EventId(row.get(0)?),
                    event_type: row.get(1)?,
                    section_name: row.get(2)?,
                    payload: row.get(3)?,
                    previous_id: prev_str.map(EventId),
                    created_at: row.get(5)?,
                })
            })
            .map_err(|e| {
                tracing::warn!(error = ?e, chapter_id = %chapter_id, "chapter: query_map event_log failed");
                e
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| {
                tracing::warn!(error = ?e, chapter_id = %chapter_id, "chapter: collecting event_log rows failed");
                e
            })?;

        // Import replay expansion: if no direct events exist for this chapter_id
        // stream, the chapter may have been inserted via import_chapter.
        // Search for an import event whose payload.chapters array contains this
        // chapter_id and expand it into synthetic open/section_append/close rows.
        if events.is_empty() {
            if let Ok(expanded) = self.expand_import_events(chapter_id, &meta) {
                if !expanded.is_empty() {
                    return Ok(ChapterReplay {
                        meta,
                        events: expanded,
                    });
                }
            }
        }

        Ok(ChapterReplay { meta, events })
    }

    /// Search `event_log` for an `import` event whose payload contains
    /// `chapter_id` and synthesise virtual `open` / `section_append` / `close`
    /// `EventRow`s for replay consumers.
    ///
    /// Returns an empty `Vec` if no matching import event is found.
    fn expand_import_events(
        &self,
        chapter_id: &ChapterId,
        meta: &ChapterMeta,
    ) -> Result<Vec<EventRow>, EventLogError> {
        // Fetch all import event rows from event_log.
        let mut stmt = self
            .conn
            .prepare(
                "SELECT event_id, payload, created_at
               FROM event_log
              WHERE event_type = 'import'
              ORDER BY event_id",
            )
            .map_err(|e| {
                tracing::warn!(error = ?e, "expand_import_events: prepare failed");
                EventLogError::Sqlite(e)
            })?;

        let rows: Vec<(String, String, i64)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .map_err(EventLogError::Sqlite)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(EventLogError::Sqlite)?;

        for (import_event_id, payload_str, created_at) in rows {
            let payload: serde_json::Value =
                serde_json::from_str(&payload_str).unwrap_or(serde_json::Value::Null);

            let empty = vec![];
            let chapters = payload
                .get("chapters")
                .and_then(|c| c.as_array())
                .unwrap_or(&empty);

            for chapter_entry in chapters {
                let cid = chapter_entry
                    .get("chapter_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if cid != chapter_id.0 {
                    continue;
                }

                // Found the chapter in this import event — synthesise EventRows.
                let mut synthetic: Vec<EventRow> = Vec::new();
                let open_id = format!("{import_event_id}_open");
                let initial_state = serde_json::json!({ "initial_state": meta.current_state });
                synthetic.push(EventRow {
                    event_id: EventId(open_id.clone()),
                    event_type: "open".to_owned(),
                    section_name: None,
                    payload: initial_state.to_string(),
                    previous_id: None,
                    created_at,
                });

                let sections_empty = vec![];
                let sections = chapter_entry
                    .get("sections")
                    .and_then(|s| s.as_array())
                    .unwrap_or(&sections_empty);
                let mut prev_id = open_id;
                for section in sections {
                    let sname = section
                        .get("section_name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let sbody = section.get("body").and_then(|v| v.as_str()).unwrap_or("");
                    let sec_id = format!("{import_event_id}_{sname}");
                    let sec_payload = serde_json::json!({ "body": sbody });
                    synthetic.push(EventRow {
                        event_id: EventId(sec_id.clone()),
                        event_type: "section_append".to_owned(),
                        section_name: Some(sname.to_owned()),
                        payload: sec_payload.to_string(),
                        previous_id: Some(EventId(prev_id.clone())),
                        created_at,
                    });
                    prev_id = sec_id;
                }

                let close_id = format!("{import_event_id}_close");
                synthetic.push(EventRow {
                    event_id: EventId(close_id),
                    event_type: "close".to_owned(),
                    section_name: None,
                    payload: "{}".to_owned(),
                    previous_id: Some(EventId(prev_id)),
                    created_at,
                });

                return Ok(synthetic);
            }
        }

        Ok(vec![])
    }

    /// Return all chapter metadata rows ordered by `opened_at` descending (newest first).
    ///
    /// Used by [`JournalCore::tail_chapters`] and [`JournalCore::chapter_ids`] to
    /// enumerate chapters without scanning individual event streams.
    ///
    /// # Errors
    ///
    /// Returns [`EventLogError::Sqlite`] on SQL failure.
    pub fn all_chapter_metas(&self) -> Result<Vec<ChapterMeta>, EventLogError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT chapter_id, schema_id, current_state, opened_at, closed_at, chapter_name
               FROM chapter_meta
              ORDER BY opened_at DESC",
            )
            .map_err(|e| {
                tracing::warn!(error = ?e, "all_chapter_metas: prepare failed");
                e
            })?;

        let rows = stmt
            .query_map([], |row| {
                Ok(ChapterMeta {
                    chapter_id: ChapterId(row.get::<_, String>(0)?),
                    schema_id: row.get(1)?,
                    current_state: row.get(2)?,
                    opened_at: row.get(3)?,
                    closed_at: row.get(4)?,
                    chapter_name: row.get(5)?,
                })
            })
            .map_err(|e| {
                tracing::warn!(error = ?e, "all_chapter_metas: query_map failed");
                e
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| {
                tracing::warn!(error = ?e, "all_chapter_metas: collecting rows failed");
                e
            })?;

        Ok(rows)
    }

    /// Count the number of `section_append` events for a specific section name in a chapter.
    ///
    /// # Arguments
    ///
    /// * `chapter_id` — target chapter.
    /// * `section_name` — section name to count.
    ///
    /// # Errors
    ///
    /// Returns [`EventLogError::Sqlite`] on SQL failure.
    pub fn section_count(
        &self,
        chapter_id: &ChapterId,
        section_name: &str,
    ) -> Result<usize, EventLogError> {
        let count: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM event_log
              WHERE stream_id = ?1
                AND event_type = 'section_append'
                AND section_name = ?2",
                rusqlite::params![chapter_id.0, section_name],
                |row| row.get(0),
            )
            .map_err(|e| {
                tracing::warn!(error = ?e, chapter_id = %chapter_id, section_name, "section_count: COUNT query failed");
                e
            })?;
        Ok(count as usize)
    }

    // ──────────────────────── import API ─────────────────────────────────

    /// Begin a rusqlite transaction on the underlying connection.
    ///
    /// Returns a [`rusqlite::Transaction`] that auto-rolls-back on `Drop` unless
    /// [`commit`](rusqlite::Transaction::commit) is called.  Use this to batch
    /// multiple `append_import` calls (and the associated `chapter_meta` inserts)
    /// into a single atomic write.
    ///
    /// # Errors
    ///
    /// Returns [`EventLogError::Sqlite`] if the transaction cannot be started
    /// (e.g. a transaction is already open on this connection).
    pub fn transaction(&mut self) -> Result<rusqlite::Transaction<'_>, EventLogError> {
        self.conn.transaction().map_err(|e| {
            tracing::warn!(error = ?e, "EventLog::transaction: failed to begin transaction");
            EventLogError::Sqlite(e)
        })
    }

    /// Insert a first-class `import` event row into `event_log` within `tx`.
    ///
    /// Also inserts one `chapter_meta` row per chapter listed in `payload.chapters`,
    /// all within the same transaction scope.  The caller must call
    /// [`tx.commit()`](rusqlite::Transaction::commit) to persist the writes.
    ///
    /// # Payload format
    ///
    /// ```json
    /// {
    ///   "source_path": "<path>",
    ///   "source_hash": "<hex>",
    ///   "migration_epoch_ms": 1234567890123,
    ///   "chapters": [
    ///     {
    ///       "chapter_id": "<id>",
    ///       "chapter_name": "<h2 literal>",
    ///       "schema_id": "journal-mcp-canonical-v1",
    ///       "sections": [{ "section_name": "<h3>", "body": "<literal>" }]
    ///     }
    ///   ]
    /// }
    /// ```
    ///
    /// # Arguments
    ///
    /// * `migration_id` — ULID string used as `stream_id` for the import event row.
    /// * `payload` — structured import payload (see above).
    /// * `tx` — active transaction to write into.
    ///
    /// # Errors
    ///
    /// Returns [`EventLogError::Sqlite`] or [`EventLogError::Json`] on failure.
    pub fn append_import(
        migration_id: &str,
        payload: &serde_json::Value,
        tx: &rusqlite::Transaction<'_>,
    ) -> Result<i64, EventLogError> {
        let event_id = Ulid::new().to_string();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(EventLogError::Time)?
            .as_millis() as i64;
        let payload_str = serde_json::to_string(payload).map_err(|e| {
            tracing::warn!(error = ?e, "append_import: failed to serialise payload");
            EventLogError::Json(e)
        })?;

        // Insert the single import event row (stream_id = migration_id).
        tx.execute(
            "INSERT INTO event_log
             (event_id, stream_id, event_type, section_name, payload, previous_id, created_at)
         VALUES (?1, ?2, 'import', NULL, ?3, NULL, ?4)",
            rusqlite::params![event_id, migration_id, payload_str, now],
        )
        .map_err(|e| {
            tracing::warn!(error = ?e, migration_id, "append_import: INSERT event_log failed");
            EventLogError::Sqlite(e)
        })?;

        // Insert one chapter_meta row per imported chapter (state = "closed").
        let empty = vec![];
        let chapters = payload
            .get("chapters")
            .and_then(|c| c.as_array())
            .unwrap_or(&empty);

        for chapter in chapters {
            let chapter_id = chapter
                .get("chapter_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let schema_id = chapter
                .get("schema_id")
                .and_then(|v| v.as_str())
                .unwrap_or("journal-mcp-canonical-v1");

            if chapter_id.is_empty() {
                tracing::warn!("append_import: skipping chapter with empty chapter_id");
                continue;
            }

            // The payload carries the heading as written; `chapter_id` is its
            // slug. Recording both is what lets the projection print the
            // original title instead of the slug twice.
            let chapter_name = chapter
                .get("chapter_name")
                .and_then(|v| v.as_str())
                .unwrap_or(chapter_id);

            tx.execute(
                "INSERT INTO chapter_meta
                 (chapter_id, schema_id, current_state, opened_at, closed_at, chapter_name)
             VALUES (?1, ?2, 'closed', ?3, ?3, ?4)",
                rusqlite::params![chapter_id, schema_id, now, chapter_name],
            )
            .map_err(|e| {
                tracing::warn!(error = ?e, chapter_id, "append_import: INSERT chapter_meta failed");
                EventLogError::Sqlite(e)
            })?;
        }

        Ok(now)
    }

    /// Check whether a chapter with the given `chapter_id` already exists in `chapter_meta`.
    ///
    /// Used by [`JournalCore::import_chapter`] to detect collisions before
    /// beginning the import transaction.
    ///
    /// # Errors
    ///
    /// Returns [`EventLogError::Sqlite`] on SQL failure.
    pub fn chapter_exists(&self, chapter_id: &ChapterId) -> Result<bool, EventLogError> {
        let count: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM chapter_meta WHERE chapter_id = ?1",
                rusqlite::params![chapter_id.0],
                |row| row.get(0),
            )
            .map_err(|e| {
                tracing::warn!(error = ?e, chapter_id = %chapter_id, "chapter_exists: COUNT query failed");
                EventLogError::Sqlite(e)
            })?;
        Ok(count > 0)
    }

    /// Compute a stable hex hash of `content` using `DefaultHasher`.
    ///
    /// Used for `source_hash` in import payloads.  Not cryptographically secure
    /// but sufficient for change-detection purposes.
    pub(crate) fn hash_content(content: &str) -> String {
        let mut hasher = DefaultHasher::new();
        content.hash(&mut hasher);
        format!("{:016x}", hasher.finish())
    }

    // ──────────────────── events export / import API ────────────────────

    /// Return every `event_log` row verbatim, ordered by `event_id`
    /// (ULID lexicographic = time order).
    ///
    /// This is the raw-transfer source for [`JournalCore::export_events`]:
    /// rows are exported exactly as stored — including first-class `import`
    /// events — so a destination store replays them identically to the
    /// source (event = source of truth; projections are derived).
    ///
    /// # Errors
    ///
    /// Returns [`EventLogError::Sqlite`] on SQL failure.
    pub fn all_event_rows(&self) -> Result<Vec<RawEventRow>, EventLogError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT event_id, stream_id, event_type, section_name, payload, previous_id, created_at
                   FROM event_log
                  ORDER BY event_id",
            )
            .map_err(|e| {
                tracing::warn!(error = ?e, "all_event_rows: prepare failed");
                EventLogError::Sqlite(e)
            })?;

        let rows = stmt
            .query_map([], |row| {
                Ok(RawEventRow {
                    event_id: row.get(0)?,
                    stream_id: row.get(1)?,
                    event_type: row.get(2)?,
                    section_name: row.get(3)?,
                    payload: row.get(4)?,
                    previous_id: row.get(5)?,
                    created_at: row.get(6)?,
                })
            })
            .map_err(EventLogError::Sqlite)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| {
                tracing::warn!(error = ?e, "all_event_rows: collecting rows failed");
                EventLogError::Sqlite(e)
            })?;
        Ok(rows)
    }

    /// Insert a raw event row inside `tx` unless an identical row already exists.
    ///
    /// Idempotency contract (events-native import):
    /// - `event_id` absent → row is inserted, returns `Ok(true)`.
    /// - `event_id` present with **identical** content → skipped, returns `Ok(false)`.
    /// - `event_id` present with **different** content → returns
    ///   [`EventLogError::EventConflict`]; the caller's transaction rolls back.
    ///
    /// # Errors
    ///
    /// [`EventLogError::EventConflict`] on a same-id / different-content pair;
    /// [`EventLogError::Sqlite`] on SQL failure.
    pub fn insert_event_row_if_absent(
        row: &RawEventRow,
        tx: &rusqlite::Transaction<'_>,
    ) -> Result<bool, EventLogError> {
        let existing: Option<RawEventRow> = tx
            .query_row(
                "SELECT event_id, stream_id, event_type, section_name, payload, previous_id, created_at
                   FROM event_log
                  WHERE event_id = ?1",
                rusqlite::params![row.event_id],
                |r| {
                    Ok(RawEventRow {
                        event_id: r.get(0)?,
                        stream_id: r.get(1)?,
                        event_type: r.get(2)?,
                        section_name: r.get(3)?,
                        payload: r.get(4)?,
                        previous_id: r.get(5)?,
                        created_at: r.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(|e| {
                tracing::warn!(error = ?e, event_id = %row.event_id, "insert_event_row_if_absent: SELECT failed");
                EventLogError::Sqlite(e)
            })?;

        if let Some(existing) = existing {
            if existing == *row {
                return Ok(false);
            }
            tracing::warn!(
                event_id = %row.event_id,
                "insert_event_row_if_absent: conflict — same event_id, different content"
            );
            return Err(EventLogError::EventConflict {
                event_id: row.event_id.clone(),
            });
        }

        tx.execute(
            "INSERT INTO event_log
             (event_id, stream_id, event_type, section_name, payload, previous_id, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                row.event_id,
                row.stream_id,
                row.event_type,
                row.section_name,
                row.payload,
                row.previous_id,
                row.created_at
            ],
        )
        .map_err(|e| {
            tracing::warn!(error = ?e, event_id = %row.event_id, "insert_event_row_if_absent: INSERT failed");
            EventLogError::Sqlite(e)
        })?;
        Ok(true)
    }

    /// Insert a `chapter_meta` row inside `tx` unless the chapter already exists.
    ///
    /// Existing chapters are left untouched (destination wins — the local
    /// store's live metadata is never overwritten by a migration batch).
    /// Returns `Ok(true)` if the row was inserted, `Ok(false)` if skipped.
    ///
    /// # Errors
    ///
    /// Returns [`EventLogError::Sqlite`] on SQL failure.
    pub fn insert_chapter_meta_if_absent(
        meta: &ChapterMeta,
        tx: &rusqlite::Transaction<'_>,
    ) -> Result<bool, EventLogError> {
        let count: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM chapter_meta WHERE chapter_id = ?1",
                rusqlite::params![meta.chapter_id.0],
                |row| row.get(0),
            )
            .map_err(|e| {
                tracing::warn!(error = ?e, chapter_id = %meta.chapter_id, "insert_chapter_meta_if_absent: COUNT failed");
                EventLogError::Sqlite(e)
            })?;
        if count > 0 {
            return Ok(false);
        }

        tx.execute(
            "INSERT INTO chapter_meta
             (chapter_id, schema_id, current_state, opened_at, closed_at, chapter_name)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                meta.chapter_id.0,
                meta.schema_id,
                meta.current_state,
                meta.opened_at,
                meta.closed_at,
                meta.chapter_name
            ],
        )
        .map_err(|e| {
            tracing::warn!(error = ?e, chapter_id = %meta.chapter_id, "insert_chapter_meta_if_absent: INSERT failed");
            EventLogError::Sqlite(e)
        })?;
        Ok(true)
    }
}
