//! `FileProjection` — HashSet-based dirty-flag skeleton for file projection.
//!
//! # ST4 scope
//!
//! This module implements the ST4 skeleton: tracking which chapters are "dirty"
//! (stale relative to the canonical [`EventLog`]) via a [`HashSet`].
//!
//! The actual file-rebuild logic (content-hash diffing, debouncing, atomic
//! write, template rendering) is **ST5 scope**.  `rebuild_chapter` in ST4
//! only clears the dirty flag and returns `Ok(())`.
//!
//! # Crux: FileProjection dirty skeleton / rebuild 分離境界
//!
//! ST4 must not include any rebuild logic.  The `rebuild_chapter`
//! implementation here is intentionally a stub: it removes the chapter from
//! the dirty set and returns immediately without touching the filesystem.
//! ST5 will replace this stub with real file IO.
//!
//! [`HashSet`]: std::collections::HashSet

use std::collections::HashSet;

use crate::projection::{private, JournalProjection, ProjectionError};
use crate::{ChapterId, ChapterReplay};

// ---------------------------------------------------------------------------
// FileProjection
// ---------------------------------------------------------------------------

/// A projection that tracks which chapters need their file representation
/// rebuilt.
///
/// # Dirty-flag management (ST4)
///
/// `FileProjection` maintains a [`HashSet`] of chapter-id strings.  When
/// `mark_dirty` is called the id is inserted; when `rebuild_chapter` is
/// called the id is removed.
///
/// # Rebuild (ST5)
///
/// The actual file-rebuild logic (atomic write, template render, content-hash
/// diffing) will be added in ST5.  In ST4 `rebuild_chapter` is a no-op stub
/// that only clears the dirty flag.
pub struct FileProjection {
    /// Set of chapter-id strings whose file representation is stale.
    ///
    /// `String` is used instead of [`ChapterId`] because [`ChapterId`] does
    /// not derive `Hash`, and adding `Hash` to `event_log.rs` is outside this
    /// subtask's file scope.
    dirty: HashSet<String>,
}

impl FileProjection {
    /// Construct a new `FileProjection` with an empty dirty set.
    pub fn new() -> Self {
        Self {
            dirty: HashSet::new(),
        }
    }

    /// Return a reference to the set of dirty chapter-id strings.
    ///
    /// Used by tests to inspect internal state without requiring `Hash` on
    /// [`ChapterId`].
    pub fn dirty_chapters(&self) -> &HashSet<String> {
        &self.dirty
    }
}

impl Default for FileProjection {
    fn default() -> Self {
        Self::new()
    }
}

// Crux: Sealed trait 外部 impl 禁止境界 — only crate-internal types may
// implement JournalProjection.  The `impl private::Sealed` below is possible
// because `projection::private` is `pub(crate)`.
impl private::Sealed for FileProjection {}

impl JournalProjection for FileProjection {
    /// Mark the chapter identified by `id` as dirty.
    ///
    /// Inserts the chapter's string id into the internal dirty set.
    /// Returns `Ok(())` always (ST4: no IO path).
    ///
    /// # Arguments
    ///
    /// * `id` — the chapter whose derived file view has become stale.
    ///
    /// # Errors
    ///
    /// Always returns `Ok(())` in ST4.  ST5 may return [`ProjectionError::Io`]
    /// when filesystem operations are introduced.
    fn mark_dirty(&mut self, id: &ChapterId) -> Result<(), ProjectionError> {
        self.dirty.insert(id.0.clone());
        Ok(())
    }

    /// Stub rebuild: removes the chapter from the dirty set.
    ///
    /// **ST4 stub** — no filesystem write is performed here.  The actual
    /// file-rebuild logic (content-hash, atomic write, template render) is
    /// ST5 scope.  Calling this method only clears the dirty flag so the
    /// chapter is no longer considered stale.
    ///
    /// # Arguments
    ///
    /// * `replay` — the full chapter replay (used in ST5 for rendering).
    ///
    /// # Errors
    ///
    /// Always returns `Ok(())` in ST4.
    fn rebuild_chapter(&mut self, replay: &ChapterReplay) -> Result<(), ProjectionError> {
        self.dirty.remove(&replay.meta.chapter_id.0);
        Ok(())
    }
}
