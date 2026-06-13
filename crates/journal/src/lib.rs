//! journal — project canonical history primitive.
//!
//! See `docs/design.md` for the full design.

#![forbid(unsafe_code)]

pub mod event_log;

pub use event_log::{
    ChapterId, ChapterMeta, ChapterReplay, EventId, EventLog, EventLogError, EventRow,
};
