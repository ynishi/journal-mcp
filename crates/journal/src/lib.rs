//! `journal` — project canonical history primitive.
//!
//! Provides [`JournalCore`] for schema-driven chapter lifecycle management,
//! [`EventLog`] for append-only persistence, [`SchemaRegistry`] for schema
//! lookup, and [`JournalProjection`] for derived-view dispatch (e.g.
//! [`FileProjection`] which renders chapters to a Markdown file).
//!
//! See `docs/design.md` for the full design.

#![forbid(unsafe_code)]

pub mod core;
pub mod event_log;
pub mod handle;
pub mod projection;
pub mod registry;
pub mod schema;

pub use core::{JournalCore, JournalError};
pub use event_log::{
    ChapterId, ChapterMeta, ChapterReplay, EventId, EventLog, EventLogError, EventRow,
};
pub use projection::{FileProjection, JournalProjection, ProjectionError};
pub use registry::{RegistryError, SchemaRegistry};
pub use schema::{
    AppendPolicy, ChapterSchema, HookAction, HookSpec, HookWarning, SchemaError, SectionSpec,
    StateSpec, TransitionRequires, TransitionSpec,
};
