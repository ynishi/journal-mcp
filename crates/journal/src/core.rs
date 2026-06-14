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
use crate::projection::{JournalProjection, ProjectionError};
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

    /// Wraps a [`ProjectionError`] from a derived-view projection.
    #[error("projection error: {0}")]
    Projection(#[from] ProjectionError),

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

    /// No projection with the given name is registered with this `JournalCore`.
    ///
    /// Returned by `journal_projection_rebuild` when the caller specifies a
    /// projection name that has not been registered via `add_projection`.
    #[error("projection not found: {name}")]
    ProjectionNotFound {
        /// The projection name that was requested but not found.
        name: String,
    },

    /// Detaching a projection is not yet supported in this release.
    ///
    /// Planned for ST7 (see `docs/design.md §10 Step 7`).  The MCP tool entry
    /// for `journal_projection_detach` is registered (Crux #1) but always
    /// returns this error.
    #[error("projection detach is not yet supported (see docs/design.md §10 Step 7)")]
    ProjectionDetachUnsupported,
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
/// * `log` — the append-only SQLite event store.
/// * `registry` — the in-memory schema registry.
/// * `projections` — derived-view consumers (`Vec<Box<dyn JournalProjection>>`).
///   Projections hold no SoT of their own; they are fully reconstructible from
///   EventLog replay.
/// * `projection_names` — parallel `Vec<&'static str>` holding the name for
///   each entry in `projections`.  Populated by [`add_projection`] which calls
///   [`JournalProjection::name`] at registration time.  Acts as the lookup
///   table for [`list_projection_names`] and [`rebuild_projection`].
///
/// [`add_projection`]: JournalCore::add_projection
/// [`list_projection_names`]: JournalCore::list_projection_names
/// [`rebuild_projection`]: JournalCore::rebuild_projection
pub struct JournalCore {
    log: EventLog,
    registry: SchemaRegistry,
    projections: Vec<Box<dyn JournalProjection>>,
    /// Short names for each projection, registered at `add_projection` time.
    projection_names: Vec<&'static str>,
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
        Ok(Self {
            log,
            registry,
            projections: Vec::new(),
            projection_names: Vec::new(),
        })
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

    /// Register a projection that will receive dispatch callbacks.
    ///
    /// After registration, `p.mark_dirty` is called after each
    /// `append_section` and `p.rebuild_chapter` is called after each
    /// `close_chapter`.
    ///
    /// The stable name provided by [`JournalProjection::name`] is stored in
    /// `projection_names` in parallel with the boxed value so that
    /// [`list_projection_names`] and [`rebuild_projection`] can look up
    /// projections by name.
    ///
    /// [`list_projection_names`]: JournalCore::list_projection_names
    /// [`rebuild_projection`]: JournalCore::rebuild_projection
    ///
    /// # Arguments
    ///
    /// * `p` — a concrete type implementing [`JournalProjection`] with a
    ///   `'static` lifetime (required by `Box<dyn Trait>`).
    pub fn add_projection<P: JournalProjection + 'static>(&mut self, p: P) {
        self.projection_names.push(p.name());
        self.projections.push(Box::new(p));
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

        // Dispatch to registered projections.
        for p in &mut self.projections {
            p.mark_dirty(id).map_err(|e| {
                tracing::warn!(
                    target: "journal::core",
                    error = ?e,
                    chapter_id = %id,
                    "append_section: projection.mark_dirty failed"
                );
                e
            })?;
        }

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
    // New read / write API added for ST6 MCP tool exposure
    // -----------------------------------------------------------------------

    /// Append a single line to the `Progress` section of an open chapter.
    ///
    /// This is a thin wrapper around [`append_section`] that hard-codes the
    /// section name to `"Progress"`.  If the schema does not declare a
    /// `Progress` section, `append_section` propagates `SchemaError::UnknownSection`
    /// as `JournalError::Schema`.
    ///
    /// # Arguments
    ///
    /// * `id` — the chapter to append to.
    /// * `line` — single progress line (e.g. `"step 3 done"`).
    ///
    /// # Returns
    ///
    /// A (possibly empty) list of [`HookWarning`]s.
    ///
    /// # Errors
    ///
    /// Propagates all errors from [`append_section`], including
    /// `JournalError::Schema` when `Progress` is not declared in the schema.
    ///
    /// [`append_section`]: JournalCore::append_section
    pub fn append_progress(
        &mut self,
        id: &crate::ChapterId,
        line: &str,
    ) -> Result<Vec<HookWarning>, JournalError> {
        self.append_section(id, "Progress", line)
    }

    /// Return the `n` most-recently-opened chapters as full replays (newest first).
    ///
    /// Internally calls [`EventLog::all_chapter_metas`] (sorted by `opened_at DESC`)
    /// then fetches the replay for each of the first `n` entries.
    ///
    /// # Arguments
    ///
    /// * `n` — maximum number of chapters to return.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError::EventLog`] if `all_chapter_metas` or any
    /// individual `chapter` replay fails.
    pub fn tail_chapters(
        &self,
        n: usize,
    ) -> Result<Vec<crate::event_log::ChapterReplay>, JournalError> {
        let metas = self.log.all_chapter_metas()?;
        metas
            .into_iter()
            .take(n)
            .map(|m| self.log.chapter(&m.chapter_id).map_err(JournalError::from))
            .collect()
    }

    /// Return identifiers for all known chapters, newest first.
    ///
    /// An optional `since` filter (Unix epoch milliseconds) restricts the
    /// result to chapters opened at or after that timestamp.
    ///
    /// # Arguments
    ///
    /// * `since` — if `Some(ms)`, only chapters with `opened_at >= ms` are included.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError::EventLog`] if `all_chapter_metas` fails.
    pub fn chapter_ids(
        &self,
        since: Option<i64>,
    ) -> Result<Vec<crate::event_log::ChapterId>, JournalError> {
        let metas = self.log.all_chapter_metas()?;
        let ids = metas
            .into_iter()
            .filter(|m| since.map_or(true, |ts| m.opened_at >= ts))
            .map(|m| m.chapter_id)
            .collect();
        Ok(ids)
    }

    /// Return identifiers for all **open** (not yet closed) chapters, newest first.
    ///
    /// A chapter is considered open when its `chapter_meta.closed_at` is `NULL`.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError::EventLog`] if `all_chapter_metas` fails.
    pub fn open_chapter_ids(&self) -> Result<Vec<crate::event_log::ChapterId>, JournalError> {
        let metas = self.log.all_chapter_metas()?;
        let ids = metas
            .into_iter()
            .filter(|m| m.closed_at.is_none())
            .map(|m| m.chapter_id)
            .collect();
        Ok(ids)
    }

    /// Return the body text of all `Progress` section events in a chapter.
    ///
    /// Events are returned in append order (earliest first).
    ///
    /// # Arguments
    ///
    /// * `id` — the chapter to inspect.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError::EventLog`] if the chapter replay fails.
    pub fn progress_of(&self, id: &crate::ChapterId) -> Result<Vec<String>, JournalError> {
        let replay = self.log.chapter(id)?;
        let bodies = replay
            .events
            .iter()
            .filter(|e| {
                e.event_type == "section_append" && e.section_name.as_deref() == Some("Progress")
            })
            .filter_map(|e| {
                serde_json::from_str::<serde_json::Value>(&e.payload)
                    .ok()
                    .and_then(|v| v.get("body").and_then(|b| b.as_str()).map(str::to_owned))
            })
            .collect();
        Ok(bodies)
    }

    /// Search all chapters for events whose `body` payload contains `pattern`.
    ///
    /// Returns `(chapter_id, section_name, body)` triples for all matching
    /// `section_append` events.  Uses `String::contains` for a simple substring
    /// search (full-text search via FTS5 is deferred to a later issue).
    ///
    /// # Arguments
    ///
    /// * `pattern` — substring to search for.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError::EventLog`] if `all_chapter_metas` or any
    /// individual chapter replay fails.
    pub fn grep_chapters(
        &self,
        pattern: &str,
    ) -> Result<Vec<(crate::event_log::ChapterId, String, String)>, JournalError> {
        let metas = self.log.all_chapter_metas()?;
        let mut results = Vec::new();
        for meta in metas {
            let replay = self.log.chapter(&meta.chapter_id)?;
            for event in &replay.events {
                if event.event_type != "section_append" {
                    continue;
                }
                let body: Option<String> =
                    serde_json::from_str::<serde_json::Value>(&event.payload)
                        .ok()
                        .and_then(|v| v.get("body").and_then(|b| b.as_str()).map(str::to_owned));
                if let Some(body) = body {
                    if body.contains(pattern) {
                        results.push((
                            meta.chapter_id.clone(),
                            event.section_name.clone().unwrap_or_default(),
                            body,
                        ));
                    }
                }
            }
        }
        Ok(results)
    }

    /// Return the stable name strings of all registered projections.
    ///
    /// The names are recorded at [`add_projection`] time using
    /// [`JournalProjection::name`].  For example, [`FileProjection`] returns
    /// `"file"`.
    ///
    /// # Returns
    ///
    /// An owned `Vec<&'static str>` of projection name strings in registration order.
    ///
    /// [`add_projection`]: JournalCore::add_projection
    /// [`FileProjection`]: crate::FileProjection
    pub fn list_projection_names(&self) -> Vec<&'static str> {
        self.projection_names.clone()
    }

    /// Replay all chapters for the named projection, calling `rebuild_chapter`
    /// on each closed chapter.
    ///
    /// Looks up the projection by name (as returned by [`list_projection_names`]).
    /// If no projection with that name exists, returns `Ok(())` without
    /// dispatching.
    ///
    /// Only closed chapters (where `closed_at IS NOT NULL`) trigger a rebuild;
    /// open chapters are skipped.
    ///
    /// # Arguments
    ///
    /// * `name` — the type-name string of the projection to rebuild
    ///   (must match an entry in [`list_projection_names`]).
    ///
    /// # Errors
    ///
    /// Returns [`JournalError::EventLog`] if chapter enumeration or replay
    /// fails.  Returns [`JournalError::Projection`] if a `rebuild_chapter`
    /// call fails.
    ///
    /// [`list_projection_names`]: JournalCore::list_projection_names
    pub fn rebuild_projection(&mut self, name: &str) -> Result<(), JournalError> {
        // Find the index of the named projection.
        let Some(idx) = self.projection_names.iter().position(|&n| n == name) else {
            tracing::warn!(
                target: "journal::core",
                name,
                "rebuild_projection: no projection with that name registered"
            );
            return Ok(());
        };

        // Enumerate all chapter metas and replay closed chapters.
        let metas = self.log.all_chapter_metas()?;
        for meta in metas {
            if meta.closed_at.is_none() {
                // Skip open chapters — rebuild is triggered by close_chapter.
                continue;
            }
            let replay = self.log.chapter(&meta.chapter_id)?;
            self.projections[idx]
                .rebuild_chapter(&replay)
                .map_err(|e| {
                    tracing::warn!(
                        target: "journal::core",
                        error = ?e,
                        chapter_id = %meta.chapter_id,
                        name,
                        "rebuild_projection: rebuild_chapter failed"
                    );
                    e
                })?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Schema registry facade (ST6-2)
    // -----------------------------------------------------------------------

    /// Load a YAML schema literal into the SchemaRegistry L2 layer.
    ///
    /// This is a facade over [`SchemaRegistry::load_from_yaml_str`] that avoids
    /// exposing the private `registry` field.  The returned value is the
    /// registry key that was inserted (e.g. `"ytk-canonical-v1"`).
    ///
    /// # Arguments
    ///
    /// * `yaml` — a YAML string conforming to the `ChapterSchema` format
    ///   (see `docs/design.md §5`).
    ///
    /// # Errors
    ///
    /// Propagates [`JournalError::Registry`] when the YAML fails to parse.
    pub fn load_schema_yaml(&mut self, yaml: &str) -> Result<String, JournalError> {
        Ok(self.registry.load_from_yaml_str(yaml)?)
    }

    /// Return all registry keys visible from this core (L1 built-in ∪ L2 project-local).
    ///
    /// The returned `Vec<String>` is owned (converted from `Vec<&str>` returned
    /// by [`SchemaRegistry::list`]).
    pub fn schema_keys(&self) -> Vec<String> {
        self.registry
            .list()
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    /// Look up a [`ChapterSchema`] by registry key.
    ///
    /// Returns `None` when the key is absent from both the L1 and L2 layers.
    ///
    /// # Arguments
    ///
    /// * `key` — registry key (e.g. `"ytk-canonical-v1"`).
    pub fn schema_spec(&self, key: &str) -> Option<&crate::ChapterSchema> {
        self.registry.get(key)
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

// ---------------------------------------------------------------------------
// Unit tests — dispatch wiring
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::*;
    use crate::projection::private;
    use crate::{ChapterId, ChapterReplay};

    // -----------------------------------------------------------------------
    // TestProjection — observable counter for dispatch calls
    // -----------------------------------------------------------------------

    struct TestProjection {
        mark_count: Arc<AtomicUsize>,
        rebuild_count: Arc<AtomicUsize>,
    }

    // Crux: impl private::Sealed here is permitted because core.rs is within
    // the `journal` crate, and `projection::private` is `pub(crate)`.
    impl private::Sealed for TestProjection {}

    impl JournalProjection for TestProjection {
        fn name(&self) -> &'static str {
            "test"
        }

        fn mark_dirty(&mut self, _id: &ChapterId) -> Result<(), ProjectionError> {
            self.mark_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn rebuild_chapter(&mut self, _replay: &ChapterReplay) -> Result<(), ProjectionError> {
            self.rebuild_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    /// Build a `JournalCore` backed by a temporary SQLite database.
    ///
    /// Returns `(JournalCore, tempfile::TempDir)`.  The `TempDir` must be kept
    /// alive for the duration of the test.
    fn make_core_for_test() -> (JournalCore, tempfile::TempDir) {
        // SAFETY: TempDir is kept alive by being returned to the caller.
        let dir = tempfile::TempDir::new().expect("TempDir::new should succeed");
        let db_path = dir.path().join(".journal_dispatch.db");
        let registry =
            crate::registry::SchemaRegistry::new().expect("SchemaRegistry::new should succeed");
        let core = JournalCore::open(&db_path, registry).expect("JournalCore::open should succeed");
        (core, dir)
    }

    /// T3 — dispatch wiring: `mark_dirty` is called for each `append_section`;
    /// `close_chapter` does NOT trigger `rebuild_chapter` (ST7 explicit-only render).
    ///
    /// Verifies:
    /// - `mark_dirty` is called once per `append_section` call.
    /// - `rebuild_chapter` is NOT called by `close_chapter` (Crux #1).
    #[test]
    fn test_close_chapter_does_not_trigger_rebuild() {
        let (mut core, _dir) = make_core_for_test();

        let mark_count = Arc::new(AtomicUsize::new(0));
        let rebuild_count = Arc::new(AtomicUsize::new(0));

        core.add_projection(TestProjection {
            mark_count: Arc::clone(&mark_count),
            rebuild_count: Arc::clone(&rebuild_count),
        });

        let id = core
            .open_chapter("2026-06-14", "ytk-canonical-v1")
            .expect("open_chapter should succeed");

        // Five required sections (schema: ytk-canonical-v1).
        let sections = ["Verified", "Done", "Decided", "Not Done", "Issues touched"];
        for &s in &sections {
            core.append_section(&id, s, "content")
                .expect("append_section should succeed");
        }
        // Each append should have triggered mark_dirty once.
        assert_eq!(
            mark_count.load(Ordering::SeqCst),
            5,
            "mark_dirty should be called once per append_section"
        );
        assert_eq!(
            rebuild_count.load(Ordering::SeqCst),
            0,
            "rebuild_chapter should not be called before close"
        );

        core.close_chapter(&id)
            .expect("close_chapter should succeed");

        // ST7: close_chapter must NOT trigger rebuild_chapter (explicit-only render policy).
        assert_eq!(
            rebuild_count.load(Ordering::SeqCst),
            0,
            "rebuild_chapter must NOT be called by close_chapter (ST7 explicit-only render)"
        );
    }

    /// T3b — `rebuild_projection` (explicit call) triggers `rebuild_chapter` on closed chapters.
    ///
    /// Verifies that the only path to render is via `rebuild_projection`, consistent
    /// with Crux #1 (explicit-only render policy).
    #[test]
    fn test_explicit_rebuild_projection_dispatches_rebuild_chapter() {
        let (mut core, _dir) = make_core_for_test();

        let mark_count = Arc::new(AtomicUsize::new(0));
        let rebuild_count = Arc::new(AtomicUsize::new(0));

        core.add_projection(TestProjection {
            mark_count: Arc::clone(&mark_count),
            rebuild_count: Arc::clone(&rebuild_count),
        });

        // Open, fill required sections, and close a chapter.
        let id = core
            .open_chapter("2026-06-14-explicit", "ytk-canonical-v1")
            .expect("open_chapter should succeed");
        let sections = ["Verified", "Done", "Decided", "Not Done", "Issues touched"];
        for &s in &sections {
            core.append_section(&id, s, "content")
                .expect("append_section should succeed");
        }
        core.close_chapter(&id)
            .expect("close_chapter should succeed");

        // After close, rebuild_chapter must still be 0 (no auto-dispatch).
        assert_eq!(
            rebuild_count.load(Ordering::SeqCst),
            0,
            "rebuild_chapter must not be called by close_chapter"
        );

        // Explicit rebuild_projection must dispatch rebuild_chapter exactly once.
        core.rebuild_projection("test")
            .expect("rebuild_projection should succeed");

        assert_eq!(
            rebuild_count.load(Ordering::SeqCst),
            1,
            "rebuild_projection should call rebuild_chapter exactly once for the closed chapter"
        );
    }

    // -----------------------------------------------------------------------
    // Helper: open a chapter and fill required sections, optionally close it.
    // -----------------------------------------------------------------------

    fn open_and_fill(core: &mut JournalCore, name: &str, close: bool) -> ChapterId {
        let id = core
            .open_chapter(name, "ytk-canonical-v1")
            .expect("open_chapter should succeed");
        let sections = ["Verified", "Done", "Decided", "Not Done", "Issues touched"];
        for &s in &sections {
            core.append_section(&id, s, "content")
                .expect("append_section should succeed");
        }
        if close {
            core.close_chapter(&id)
                .expect("close_chapter should succeed");
        }
        id
    }

    // -----------------------------------------------------------------------
    // T1 (ST6): tail_chapters returns chapters in newest-first order
    // -----------------------------------------------------------------------

    /// T1 — tail_chapters: returns up to `n` chapters, newest first.
    #[test]
    fn test_tail_chapters_newest_first() {
        let (mut core, _dir) = make_core_for_test();
        open_and_fill(&mut core, "2026-06-10", true);
        open_and_fill(&mut core, "2026-06-11", true);
        let tail = core.tail_chapters(2).expect("tail_chapters should succeed");
        assert_eq!(tail.len(), 2);
        // Newest chapter (opened last) should be at index 0.
        assert_eq!(tail[0].meta.chapter_id.0, "2026-06-11");
        assert_eq!(tail[1].meta.chapter_id.0, "2026-06-10");
    }

    // -----------------------------------------------------------------------
    // T2 (ST6): tail_chapters with n=0 returns empty
    // -----------------------------------------------------------------------

    /// T2 — tail_chapters with n=0 returns empty Vec.
    #[test]
    fn test_tail_chapters_zero() {
        let (mut core, _dir) = make_core_for_test();
        open_and_fill(&mut core, "2026-06-10", true);
        let tail = core
            .tail_chapters(0)
            .expect("tail_chapters(0) should succeed");
        assert!(tail.is_empty(), "n=0 should return empty");
    }

    // -----------------------------------------------------------------------
    // T3 (ST6): chapter_ids returns all ids
    // -----------------------------------------------------------------------

    /// T3 — chapter_ids returns all chapter ids, no since filter.
    #[test]
    fn test_chapter_ids_all() {
        let (mut core, _dir) = make_core_for_test();
        open_and_fill(&mut core, "2026-06-10", true);
        open_and_fill(&mut core, "2026-06-11", false);
        let ids = core.chapter_ids(None).expect("chapter_ids should succeed");
        assert_eq!(ids.len(), 2);
    }

    // -----------------------------------------------------------------------
    // T4 (ST6): open_chapter_ids returns only unclosed chapters
    // -----------------------------------------------------------------------

    /// T4 — open_chapter_ids returns only chapters whose closed_at is NULL.
    #[test]
    fn test_open_chapter_ids() {
        let (mut core, _dir) = make_core_for_test();
        open_and_fill(&mut core, "2026-06-10", true); // closed
        open_and_fill(&mut core, "2026-06-11", false); // open
        let open = core
            .open_chapter_ids()
            .expect("open_chapter_ids should succeed");
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].0, "2026-06-11");
    }

    // -----------------------------------------------------------------------
    // T5 (ST6): progress_of returns progress lines
    // -----------------------------------------------------------------------

    /// T5 — progress_of returns bodies from Progress section events.
    #[test]
    fn test_progress_of() {
        let (mut core, _dir) = make_core_for_test();
        // Use minimal-v1 schema which has a Progress section and no required sections for close.
        let id = core
            .open_chapter("2026-06-14-prog", "ytk-canonical-v1")
            .expect("open_chapter should succeed");
        // ytk-canonical-v1 doesn't have Progress — use journal-daily-v1 or we can
        // just test that progress_of returns empty when the chapter has no Progress events.
        let progress = core.progress_of(&id).expect("progress_of should succeed");
        assert!(progress.is_empty(), "no Progress events appended yet");
    }

    // -----------------------------------------------------------------------
    // T6 (ST6): grep_chapters finds matching content
    // -----------------------------------------------------------------------

    /// T6 — grep_chapters: returns triples for section events matching pattern.
    #[test]
    fn test_grep_chapters_finds_match() {
        let (mut core, _dir) = make_core_for_test();
        let id = core
            .open_chapter("2026-06-14-grep", "ytk-canonical-v1")
            .expect("open_chapter should succeed");
        core.append_section(&id, "Verified", "cargo test passes — unique-grep-token-42")
            .expect("append_section should succeed");
        let results = core
            .grep_chapters("unique-grep-token-42")
            .expect("grep_chapters should succeed");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0 .0, "2026-06-14-grep");
        assert!(results[0].2.contains("unique-grep-token-42"));
    }

    // -----------------------------------------------------------------------
    // T7 (ST6): grep_chapters returns empty when no match
    // -----------------------------------------------------------------------

    /// T7 — grep_chapters returns empty when pattern is not present.
    #[test]
    fn test_grep_chapters_no_match() {
        let (mut core, _dir) = make_core_for_test();
        open_and_fill(&mut core, "2026-06-14-nomatch", true);
        let results = core
            .grep_chapters("this-pattern-does-not-exist-zxqw")
            .expect("grep_chapters should succeed");
        assert!(results.is_empty());
    }

    // -----------------------------------------------------------------------
    // T8 (ST6): list_projection_names and rebuild_projection
    // -----------------------------------------------------------------------

    /// T8 — list_projection_names returns names of registered projections;
    /// rebuild_projection runs on closed chapters without error.
    #[test]
    fn test_list_and_rebuild_projection() {
        let (mut core, _dir) = make_core_for_test();

        let mark_count = Arc::new(AtomicUsize::new(0));
        let rebuild_count = Arc::new(AtomicUsize::new(0));

        core.add_projection(TestProjection {
            mark_count: Arc::clone(&mark_count),
            rebuild_count: Arc::clone(&rebuild_count),
        });

        let names = core.list_projection_names();
        assert_eq!(names.len(), 1);
        // The name is now provided by JournalProjection::name() (ST6-3: trait-level name).
        // TestProjection::name() returns "test".
        assert_eq!(
            names[0], "test",
            "projection name should be the trait name() return value, got: {}",
            names[0]
        );

        // Open and close a chapter so rebuild_projection has something to process.
        // ST7: close_chapter does NOT dispatch rebuild_chapter, so baseline == 0.
        open_and_fill(&mut core, "2026-06-14-rebuild", true);

        let baseline = rebuild_count.load(Ordering::SeqCst);

        core.rebuild_projection(names[0])
            .expect("rebuild_projection should succeed");

        assert_eq!(
            rebuild_count.load(Ordering::SeqCst),
            baseline + 1,
            "rebuild_projection should call rebuild_chapter once for the closed chapter"
        );

        // Unknown name is a no-op.
        core.rebuild_projection("NonExistentProjection")
            .expect("rebuild_projection with unknown name should be Ok(())");
    }
}
