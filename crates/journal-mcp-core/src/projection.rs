//! `JournalProjection` sealed trait — derived-view dispatch for journal chapters.
//!
//! # Design
//!
//! [`JournalProjection`] is a sealed trait (BP-6.1) that `JournalCore` calls
//! after each `append_section` and `close_chapter` to keep derived views
//! (e.g. rendered markdown files) in sync with the canonical [`EventLog`].
//!
//! # Sealed trait
//!
//! Only types within this crate can implement [`JournalProjection`].  External
//! crates cannot satisfy the [`private::Sealed`] super-trait requirement:
//!
//! ```compile_fail
//! use journal_mcp_core::JournalProjection;
//! struct External;
//! impl JournalProjection for External {
//!     fn name(&self) -> &'static str { "external" }
//!     fn mark_dirty(
//!         &mut self,
//!         _id: &journal_mcp_core::ChapterId,
//!     ) -> Result<(), journal_mcp_core::ProjectionError> {
//!         Ok(())
//!     }
//!     fn rebuild_chapter(
//!         &mut self,
//!         _replay: &journal_mcp_core::ChapterReplay,
//!     ) -> Result<(), journal_mcp_core::ProjectionError> {
//!         Ok(())
//!     }
//! }
//! ```
//!
//! [`EventLog`]: crate::EventLog

pub mod file;
pub mod fts5;

pub use file::FileProjection;
pub use fts5::FTS5Projection;

// ---------------------------------------------------------------------------
// Sealed module (Crux: Sealed trait 外部 impl 禁止境界)
// ---------------------------------------------------------------------------

/// Sealing supertrait — crate-internal only, invisible to external crates.
///
/// `pub(crate)` allows implementations within any module of this crate
/// (including `projection::file`) while remaining invisible to downstream
/// crates, which preserves the sealed-trait invariant.
pub(crate) mod private {
    /// Marker trait that seals [`super::JournalProjection`].
    ///
    /// Only types within this crate can implement `Sealed`, because `private`
    /// is crate-private.  External crates cannot name this trait.
    pub trait Sealed {}
}

// ---------------------------------------------------------------------------
// ProjectionError
// ---------------------------------------------------------------------------

/// Errors returned by [`JournalProjection`] methods.
///
/// Wraps the underlying failure source.  File-based projections (e.g.
/// [`FileProjection`]) produce [`ProjectionError::Io`]; SQLite-backed
/// projections (e.g. [`FTS5Projection`]) produce [`ProjectionError::Sql`].
#[derive(Debug, thiserror::Error)]
pub enum ProjectionError {
    /// An IO error, returned by file-based projections.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// A SQLite operation error, returned by SQLite-backed projections.
    #[error("sqlite error: {0}")]
    Sql(#[from] rusqlite::Error),
    /// A JSON serialization or deserialization error, returned by projections
    /// that parse event payloads.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

// ---------------------------------------------------------------------------
// JournalProjection trait (sealed)
// ---------------------------------------------------------------------------

/// A derived-view consumer that `JournalCore` notifies after each chapter
/// mutation.
///
/// Implementations receive two callbacks:
///
/// * [`mark_dirty`](JournalProjection::mark_dirty) — called after a section is
///   appended; records that the chapter's derived view is stale.
/// * [`rebuild_chapter`](JournalProjection::rebuild_chapter) — called after a
///   chapter is closed; reconstructs the derived view from the full replay.
///
/// # Sealed
///
/// This trait is sealed (see the module-level doc).  Only types inside the
/// `journal-mcp-core` crate may implement it.
pub trait JournalProjection: private::Sealed + Send {
    /// Stable, lowercase identifier for this projection type.
    ///
    /// Used by [`JournalCore::rebuild_projection`] and
    /// [`JournalCore::list_projection_names`] for named lookup.
    ///
    /// # Returns
    ///
    /// A `'static str` naming this projection, e.g. `"file"` for
    /// [`FileProjection`].  The name must be unique among all projections
    /// registered with a given `JournalCore`.
    fn name(&self) -> &'static str;

    /// Mark the chapter identified by `id` as requiring a rebuild.
    ///
    /// Called by [`JournalCore::append_section`] after a successful
    /// `EventLog` write.
    ///
    /// # Arguments
    ///
    /// * `id` — the chapter whose derived view has become stale.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectionError`] on IO failure (ST5+); ST4 always returns
    /// `Ok(())`.
    fn mark_dirty(&mut self, id: &crate::ChapterId) -> Result<(), ProjectionError>;

    /// Rebuild the derived view for the chapter described by `replay`.
    ///
    /// Called by [`JournalCore::close_chapter`] after a successful
    /// `EventLog::append_close` write, passing the complete post-close replay.
    ///
    /// # Arguments
    ///
    /// * `replay` — full chapter replay including the close event.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectionError`] on IO failure (ST5+); ST4 stubs return
    /// `Ok(())` after clearing the dirty flag.
    fn rebuild_chapter(&mut self, replay: &crate::ChapterReplay) -> Result<(), ProjectionError>;
}
