//! journal — project canonical history primitive.
//!
//! See `docs/design.md` for the full design.

#![forbid(unsafe_code)]

pub mod event_log;
pub mod registry;
pub mod schema;

pub use event_log::{
    ChapterId, ChapterMeta, ChapterReplay, EventId, EventLog, EventLogError, EventRow,
};
pub use registry::{RegistryError, SchemaRegistry};
pub use schema::{
    AppendPolicy, ChapterSchema, SchemaError, SectionSpec, StateSpec, TransitionRequires,
    TransitionSpec,
};
