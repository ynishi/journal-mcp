//! `JournalCore` — schema-driven state transition engine for chapter management.
//!
//! `JournalCore` is a pure *schema interpretation* layer that sits above the
//! append-only [`EventLog`](crate::EventLog) (ST1) and the schema/registry
//! infrastructure (ST2).  It never owns or writes persistent state directly;
//! all durable operations are delegated to `EventLog`.
//!
//! # Crux invariants enforced here
//!
//! 1. **EventLog SoT append delegation** — every `append_*`-style operation
//!    on `JournalCore` calls through to `self.log.append_*` and uses no other
//!    persistent-storage path.
//!
//! 2. **JournalCore schema-only interpretation boundary** — `JournalCore`
//!    holds only `log: EventLog` and `registry: SchemaRegistry`; it contains
//!    no `rusqlite::Connection`, no raw SQL, and no direct table writes.
//!    Chapter state is always *derived* from `EventLog` via schema
//!    interpretation (`self.log.chapter(id)?.meta.current_state`).

use std::path::Path;

use thiserror::Error;

use crate::event_log::{EventLog, EventLogError};
use crate::registry::{RegistryError, SchemaRegistry};
use crate::schema::{AppendPolicy, HookWarning, SchemaError};
use crate::ChapterId;

// ---------------------------------------------------------------------------
// JournalError
// ---------------------------------------------------------------------------

/// Errors returned by [`JournalCore`] operations.
#[derive(Debug, Error)]
pub enum JournalError {
    /// Wraps a [`SchemaError`] from the schema/registry layer.
    #[error("schema error: {0}")]
    Schema(#[from] SchemaError),

    /// Wraps an [`EventLogError`] from the event-log layer.
    #[error("event log error: {0}")]
    EventLog(#[from] EventLogError),

    /// Wraps a [`RegistryError`] from the schema registry.
    #[error("registry error: {0}")]
    Registry(#[from] RegistryError),

    /// The requested schema id is not present in the registry.
    #[error("schema not found: {schema_id}")]
    SchemaNotFound {
        /// The schema id that was requested.
        schema_id: String,
    },

    /// The schema declares no initial state.
    #[error("no initial state in schema {schema_id}")]
    NoInitialState {
        /// The schema id that is missing an initial state.
        schema_id: String,
    },

    /// No state-machine transition exists for `(current, event)`.
    #[error("no transition from state '{current}' on event '{event}'")]
    NoTransition {
        /// The chapter's current state.
        current: String,
        /// The event name that was attempted.
        event: String,
    },

    /// The `AppendOnce` policy was violated: a second append was attempted.
    #[error("append_once policy violated for section '{section}'")]
    AppendOncePolicy {
        /// The section whose policy was violated.
        section: String,
    },

    /// The chapter cannot be closed because a required section is absent.
    #[error("close requires section '{section}' present but absent")]
    RequiresSectionsPresent {
        /// The first missing required section.
        section: String,
    },

    /// The chapter cannot be closed because a required section has an empty body.
    #[error("close requires section '{section}' non-empty but body empty")]
    RequiresSectionsNonEmpty {
        /// The section whose body is empty.
        section: String,
    },
}

// ---------------------------------------------------------------------------
// JournalCore
// ---------------------------------------------------------------------------

/// Schema-driven engine for opening, populating, and closing journal chapters.
///
/// `JournalCore` does **not** own persistent state directly.  All durable
/// writes flow through the embedded [`EventLog`] (`self.log`), and all
/// chapter state is derived by replaying events via `self.log.chapter(id)`.
///
/// # Fields
///
/// Only two fields are permitted (Crux: JournalCore schema-only interpretation
/// boundary):
///
/// * `log` — the append-only SQLite event store.
/// * `registry` — the in-memory schema registry.
pub struct JournalCore {
    log: EventLog,
    registry: SchemaRegistry,
}

impl JournalCore {
    /// Open (or create) a `JournalCore` backed by a database at `db_path`.
    ///
    /// # Arguments
    ///
    /// * `db_path` — filesystem path for the SQLite database.
    /// * `registry` — pre-constructed schema registry to use.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError::EventLog`] if the database cannot be opened.
    pub fn open(db_path: &Path, registry: SchemaRegistry) -> Result<Self, JournalError> {
        let log = EventLog::open(db_path).map_err(|e| {
            tracing::warn!(
                target: "journal::core",
                error = ?e,
                path = ?db_path,
                "JournalCore::open failed"
            );
            e
        })?;
        Ok(Self { log, registry })
    }

    /// Open a new chapter under `name` governed by `schema_id`.
    ///
    /// Looks up the schema in the registry, derives the initial state, then
    /// delegates to [`EventLog::append_chapter_open`].
    ///
    /// # Arguments
    ///
    /// * `name` — chapter identifier (e.g. `"2026-06-13"`).
    /// * `schema_id` — registry key (e.g. `"ytk-canonical-v1"`).
    ///
    /// # Errors
    ///
    /// * [`JournalError::SchemaNotFound`] — schema not in registry.
    /// * [`JournalError::NoInitialState`] — schema has no initial state declared.
    /// * [`JournalError::EventLog`] — underlying EventLog failure.
    pub fn open_chapter(&mut self, name: &str, schema_id: &str) -> Result<ChapterId, JournalError> {
        let schema = self.registry.get(schema_id).ok_or_else(|| {
            tracing::warn!(
                target: "journal::core",
                schema_id,
                "open_chapter: schema not found in registry"
            );
            JournalError::SchemaNotFound {
                schema_id: schema_id.to_owned(),
            }
        })?;

        let initial = schema.initial_state().ok_or_else(|| {
            tracing::warn!(
                target: "journal::core",
                schema_id,
                "open_chapter: schema has no initial state"
            );
            JournalError::NoInitialState {
                schema_id: schema_id.to_owned(),
            }
        })?;

        // Crux: EventLog SoT append delegation — ALL persistent writes go through
        // self.log.append_chapter_open; no direct SQL here.
        self.log.append_chapter_open(name, schema_id, initial)?;
        Ok(ChapterId(name.to_owned()))
    }

    /// Append a section row to an existing chapter.
    ///
    /// Validates the state-machine transition, enforces `AppendOnce` policy,
    /// runs section hooks, then delegates to [`EventLog::append_section`].
    ///
    /// # Arguments
    ///
    /// * `id` — the chapter to append to.
    /// * `section_name` — name of the section (e.g. `"Verified"`).
    /// * `body` — text content of the section row.
    ///
    /// # Returns
    ///
    /// A (possibly empty) list of [`HookWarning`]s fired by the section's hooks.
    ///
    /// # Errors
    ///
    /// * [`JournalError::EventLog`] — chapter not found or SQLite failure.
    /// * [`JournalError::SchemaNotFound`] — schema no longer in registry.
    /// * [`JournalError::NoTransition`] — no transition from current state on `"append_section"`.
    /// * [`JournalError::AppendOncePolicy`] — section already has a row and policy is `AppendOnce`.
    /// * [`JournalError::Schema`] — section not declared in schema.
    pub fn append_section(
        &mut self,
        id: &ChapterId,
        section_name: &str,
        body: &str,
    ) -> Result<Vec<HookWarning>, JournalError> {
        // Derive current state by replaying from EventLog (Crux 3: schema interpretation).
        let replay = self.log.chapter(id).map_err(|e| {
            tracing::warn!(
                target: "journal::core",
                error = ?e,
                chapter_id = %id,
                "append_section: chapter replay failed"
            );
            e
        })?;

        let current_state = replay.meta.current_state.clone();
        let schema_id = replay.meta.schema_id.clone();

        let schema = self.registry.get(&schema_id).ok_or_else(|| {
            tracing::warn!(
                target: "journal::core",
                schema_id = %schema_id,
                "append_section: schema not found"
            );
            JournalError::SchemaNotFound {
                schema_id: schema_id.clone(),
            }
        })?;

        // Validate state-machine transition.
        let transition = schema
            .transition(&current_state, "append_section")
            .map_err(|e| {
                tracing::warn!(
                    target: "journal::core",
                    error = ?e,
                    chapter_id = %id,
                    current_state = %current_state,
                    "append_section: no valid transition"
                );
                JournalError::NoTransition {
                    current: current_state.clone(),
                    event: "append_section".to_owned(),
                }
            })?;

        let next_state = transition.to.clone();

        // Validate section spec and enforce AppendOnce policy.
        let section_spec = schema.section(section_name).map_err(|e| {
            tracing::warn!(
                target: "journal::core",
                error = ?e,
                section = section_name,
                "append_section: section not found in schema"
            );
            e
        })?;

        if section_spec.append_policy == Some(AppendPolicy::AppendOnce) {
            let count = self.log.section_count(id, section_name).map_err(|e| {
                tracing::warn!(
                    target: "journal::core",
                    error = ?e,
                    chapter_id = %id,
                    section = section_name,
                    "append_section: section_count query failed"
                );
                e
            })?;
            if count > 0 {
                tracing::warn!(
                    target: "journal::core",
                    chapter_id = %id,
                    section = section_name,
                    "append_section: AppendOnce policy violated"
                );
                return Err(JournalError::AppendOncePolicy {
                    section: section_name.to_owned(),
                });
            }
        }

        // Run hooks before persisting (hooks inspect the body to be appended).
        let warnings = schema.run_hooks(section_name, body);

        // Crux: EventLog SoT append delegation — single persistent write path.
        self.log
            .append_section(id, section_name, body, &next_state, None)
            .map_err(|e| {
                tracing::warn!(
                    target: "journal::core",
                    error = ?e,
                    chapter_id = %id,
                    section = section_name,
                    "append_section: EventLog::append_section failed"
                );
                e
            })?;

        Ok(warnings)
    }

    /// Close a chapter after validating all `requires` preconditions.
    ///
    /// Checks that every section listed in the transition's
    /// `requires.sections_present` has at least one row, and that every
    /// section in `requires.sections_non_empty` has a non-empty body.
    /// Then delegates to [`EventLog::append_close`].
    ///
    /// # Arguments
    ///
    /// * `id` — the chapter to close.
    ///
    /// # Errors
    ///
    /// * [`JournalError::EventLog`] — chapter not found or SQLite failure.
    /// * [`JournalError::SchemaNotFound`] — schema not in registry.
    /// * [`JournalError::NoTransition`] — no `close_chapter` transition from current state.
    /// * [`JournalError::RequiresSectionsPresent`] — a required section is absent.
    /// * [`JournalError::RequiresSectionsNonEmpty`] — a required section has an empty body.
    pub fn close_chapter(&mut self, id: &ChapterId) -> Result<(), JournalError> {
        // Derive state by replaying from EventLog (Crux 3).
        let replay = self.log.chapter(id).map_err(|e| {
            tracing::warn!(
                target: "journal::core",
                error = ?e,
                chapter_id = %id,
                "close_chapter: chapter replay failed"
            );
            e
        })?;

        let current_state = replay.meta.current_state.clone();
        let schema_id = replay.meta.schema_id.clone();

        let schema = self.registry.get(&schema_id).ok_or_else(|| {
            tracing::warn!(
                target: "journal::core",
                schema_id = %schema_id,
                "close_chapter: schema not found"
            );
            JournalError::SchemaNotFound {
                schema_id: schema_id.clone(),
            }
        })?;

        // Validate transition exists (must have a close_chapter transition).
        let transition = schema
            .transition(&current_state, "close_chapter")
            .map_err(|e| {
                tracing::warn!(
                    target: "journal::core",
                    error = ?e,
                    chapter_id = %id,
                    current_state = %current_state,
                    "close_chapter: no valid transition"
                );
                JournalError::NoTransition {
                    current: current_state.clone(),
                    event: "close_chapter".to_owned(),
                }
            })?;

        let next_state = transition.to.clone();

        // Check sections_present and sections_non_empty requires.
        if let Some(requires) = &transition.requires {
            self.check_sections_present(id, &replay.events, &requires.sections_present)?;
            self.check_sections_non_empty(id, &replay.events, &requires.sections_non_empty)?;
        }

        // Crux: EventLog SoT append delegation — single persistent write path.
        self.log.append_close(id, &next_state).map_err(|e| {
            tracing::warn!(
                target: "journal::core",
                error = ?e,
                chapter_id = %id,
                "close_chapter: EventLog::append_close failed"
            );
            e
        })?;

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Verify that each section in `required` has at least one `section_append`
    /// event in the chapter's replayed events.
    fn check_sections_present(
        &self,
        id: &ChapterId,
        events: &[crate::event_log::EventRow],
        required: &[String],
    ) -> Result<(), JournalError> {
        for section in required {
            let present = events.iter().any(|e| {
                e.event_type == "section_append"
                    && e.section_name.as_deref() == Some(section.as_str())
            });
            if !present {
                tracing::warn!(
                    target: "journal::core",
                    chapter_id = %id,
                    section = %section,
                    "close_chapter: required section absent"
                );
                return Err(JournalError::RequiresSectionsPresent {
                    section: section.clone(),
                });
            }
        }
        Ok(())
    }

    /// Verify that each section in `required` has at least one `section_append`
    /// event with a non-empty body in the chapter's replayed events.
    fn check_sections_non_empty(
        &self,
        id: &ChapterId,
        events: &[crate::event_log::EventRow],
        required: &[String],
    ) -> Result<(), JournalError> {
        for section in required {
            let non_empty = events.iter().any(|e| {
                e.event_type == "section_append"
                    && e.section_name.as_deref() == Some(section.as_str())
                    && !body_is_empty(&e.payload)
            });
            if !non_empty {
                tracing::warn!(
                    target: "journal::core",
                    chapter_id = %id,
                    section = %section,
                    "close_chapter: required section non-empty check failed"
                );
                return Err(JournalError::RequiresSectionsNonEmpty {
                    section: section.clone(),
                });
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Private utility
// ---------------------------------------------------------------------------

/// Extract the `body` field from an event payload JSON and check whether it
/// is empty.  Returns `true` when the body is absent or an empty string.
fn body_is_empty(payload: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) else {
        return true;
    };
    match v.get("body").and_then(|b| b.as_str()) {
        Some(s) => s.trim().is_empty(),
        None => true,
    }
}
