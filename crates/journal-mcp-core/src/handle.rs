//! `ChapterHandle<S: ChapterState>` — compile-time typestate guard for chapter
//! state transitions (BP-6.1 sealed trait + BP-6.2 PhantomData typestate).
//!
//! # Design
//!
//! [`ChapterHandle`] is parameterised over a *state marker* type `S` that
//! implements the sealed [`ChapterState`] trait.  Three marker structs
//! (`Open`, `Appending`, `Closed`) represent the three observable states of a
//! chapter's lifecycle.
//!
//! Transition methods consume `self` (move semantics) so the *old* state
//! becomes inaccessible after a transition, enforcing a linear state machine
//! at compile time.
//!
//! # Crux: ChapterHandle typestate compile-time guard
//!
//! `impl ChapterHandle<Closed>` **intentionally omits** `append_section` and
//! `close` methods.  Any attempt to call those methods on a `Closed` handle
//! produces a compile error, not a runtime error.
//!
//! ```compile_fail
//! use journal_mcp_core::handle::{ChapterHandle, Closed};
//! fn _illegal(h: ChapterHandle<Closed>) {
//!     let _ = h.close(); // no method named `close` on `ChapterHandle<Closed>`
//! }
//! ```

use std::marker::PhantomData;
use std::sync::Arc;

use crate::schema::ChapterSchema;
use crate::ChapterId;

// ---------------------------------------------------------------------------
// Sealed trait machinery (BP-6.1)
// ---------------------------------------------------------------------------

mod private {
    /// Sealing supertrait — invisible outside this module, so external code
    /// cannot implement `ChapterState` for new types.
    pub trait Sealed {}
}

/// Marker trait for chapter state types.
///
/// Sealed via [`private::Sealed`]; only `Open`, `Appending`, and `Closed`
/// implement it, making the set of valid states closed.
///
/// Unused at runtime under the Option β design (core.rs routes calls through
/// EventLog replay, not through ChapterHandle methods), but the typestate
/// machinery is the Crux compile-time guard and must remain present.
#[allow(dead_code)]
pub(crate) trait ChapterState: private::Sealed {}

// ---------------------------------------------------------------------------
// State marker structs
// ---------------------------------------------------------------------------

/// Marker for the `open` state: a chapter has been created but no sections
/// have been appended yet.
#[allow(dead_code)]
pub(crate) struct Open;

/// Marker for the `appending` state: at least one section has been appended.
#[allow(dead_code)]
pub(crate) struct Appending;

/// Marker for the `closed` state: the chapter has been closed.
///
/// Handles in this state do **not** expose `append_section` or `close`
/// methods — attempting to call them is a compile-time error.
#[allow(dead_code)]
pub(crate) struct Closed;

impl private::Sealed for Open {}
impl private::Sealed for Appending {}
impl private::Sealed for Closed {}

impl ChapterState for Open {}
impl ChapterState for Appending {}
impl ChapterState for Closed {}

// ---------------------------------------------------------------------------
// ChapterHandle
// ---------------------------------------------------------------------------

/// A handle to an in-progress chapter, parameterised by its current state `S`.
///
/// Transition methods move `self`, invalidating the old handle and returning
/// one in the new state.  This makes illegal transitions (e.g. appending to a
/// closed chapter) a compile-time error rather than a runtime panic.
///
/// `ChapterHandle` is crate-internal (`pub(crate)`).  External callers
/// interact with the chapter lifecycle through [`JournalCore`](crate::JournalCore).
///
/// Under the Option β design, `core.rs` routes all chapter lifecycle calls
/// through `EventLog` replay rather than through `ChapterHandle` methods at
/// runtime.  The struct and its `impl` blocks are therefore intentionally
/// unused at runtime, but the typestate machinery is the Crux compile-time
/// guard and must remain present.
#[allow(dead_code)]
pub(crate) struct ChapterHandle<S: ChapterState> {
    id: ChapterId,
    schema: Arc<ChapterSchema>,
    _state: PhantomData<S>,
}

// ---------------------------------------------------------------------------
// impl ChapterHandle<Open>
// ---------------------------------------------------------------------------

#[allow(dead_code)]
impl ChapterHandle<Open> {
    /// Construct a new handle in the `Open` state.
    ///
    /// # Arguments
    ///
    /// * `id` — the chapter's identifier.
    /// * `schema` — the parsed schema governing this chapter.
    pub(crate) fn new(id: ChapterId, schema: Arc<ChapterSchema>) -> Self {
        Self {
            id,
            schema,
            _state: PhantomData,
        }
    }

    /// Transition from `Open` to `Appending` (consuming this handle).
    ///
    /// Returns a new `ChapterHandle<Appending>` that carries the same `id`
    /// and `schema`.
    pub(crate) fn start_appending(self) -> ChapterHandle<Appending> {
        ChapterHandle {
            id: self.id,
            schema: self.schema,
            _state: PhantomData,
        }
    }
}

// ---------------------------------------------------------------------------
// impl ChapterHandle<Appending>
// ---------------------------------------------------------------------------

#[allow(dead_code)]
impl ChapterHandle<Appending> {
    /// Transition from `Appending` back to `Appending` after a section append.
    ///
    /// Returns `self` unchanged so callers can chain multiple appends without
    /// consuming the handle permanently.
    ///
    /// # Arguments
    ///
    /// * `_section_name` — reserved for future use; not validated here (validation
    ///   is performed by [`JournalCore`](crate::JournalCore)).
    pub(crate) fn after_append(self) -> ChapterHandle<Appending> {
        self
    }

    /// Transition from `Appending` to `Closed` (consuming this handle).
    ///
    /// Returns a `ChapterHandle<Closed>`.
    pub(crate) fn close(self) -> ChapterHandle<Closed> {
        ChapterHandle {
            id: self.id,
            schema: self.schema,
            _state: PhantomData,
        }
    }
}

// ---------------------------------------------------------------------------
// impl ChapterHandle<Closed>
//
// Crux: `append_section` and `close` are intentionally absent here.
// Any caller attempting to append to or close a `Closed` handle will receive
// a compile-time error.
// ---------------------------------------------------------------------------

#[allow(dead_code)]
impl ChapterHandle<Closed> {
    /// Returns the chapter identifier associated with this closed handle.
    ///
    /// This is the only operation permitted on a closed chapter handle.
    pub(crate) fn id(&self) -> &ChapterId {
        &self.id
    }
}
