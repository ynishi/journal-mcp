//! EventLog — append-only SQLite-backed canonical chapter history.
//!
//! Implements the schema from `docs/design.md §8.1` with PRAGMA settings from `§9.3`.
//! The `event_log` table is immutable at the database level via BEFORE UPDATE/BEFORE DELETE
//! triggers. `chapter_meta` has no triggers so state-transition UPDATEs succeed.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
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
    closed_at     INTEGER
) STRICT";

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
    /// * `schema_id` — schema identifier used to interpret this chapter's events.
    /// * `initial_state` — initial state for the chapter's state machine.
    ///
    /// # Errors
    ///
    /// Returns [`EventLogError::Sqlite`] on SQL failure, [`EventLogError::Time`] if the
    /// system clock is before the Unix epoch, or [`EventLogError::Json`] on serialisation failure.
    pub fn append_chapter_open(
        &mut self,
        chapter_id: &str,
        schema_id: &str,
        initial_state: &str,
    ) -> Result<EventId, EventLogError> {
        let event_id = Self::new_event_id();
        let now = Self::now_ms()?;
        let payload = serde_json::json!({ "initial_state": initial_state }).to_string();

        self.conn
            .execute(
                "INSERT INTO event_log
                 (event_id, stream_id, event_type, section_name, payload, previous_id, created_at)
             VALUES (?1, ?2, 'open', NULL, ?3, NULL, ?4)",
                rusqlite::params![event_id, chapter_id, payload, now],
            )
            .map_err(|e| {
                tracing::warn!(error = ?e, chapter_id, "append_chapter_open: INSERT event_log failed");
                e
            })?;

        self.conn
            .execute(
                "INSERT INTO chapter_meta
                 (chapter_id, schema_id, current_state, opened_at, closed_at)
             VALUES (?1, ?2, ?3, ?4, NULL)",
                rusqlite::params![chapter_id, schema_id, initial_state, now],
            )
            .map_err(|e| {
                tracing::warn!(error = ?e, chapter_id, "append_chapter_open: INSERT chapter_meta failed");
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
    /// # Errors
    ///
    /// Returns [`EventLogError::ChapterNotFound`] if no `chapter_meta` row exists for
    /// `chapter_id`, or [`EventLogError::Sqlite`] on SQL failure.
    pub fn chapter(&self, chapter_id: &ChapterId) -> Result<ChapterReplay, EventLogError> {
        // Fetch chapter_meta row.
        let meta = self
            .conn
            .query_row(
                "SELECT chapter_id, schema_id, current_state, opened_at, closed_at
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

        Ok(ChapterReplay { meta, events })
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
}
