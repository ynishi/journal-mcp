//! `JsonProjection` — machine-readable JSON dump of all chapters.
//!
//! Writes the full chapter set + events to `workspace/journal.json` (or a
//! caller-supplied path) as a single structured JSON document with a
//! stable schema versioning header.  Designed for programmatic consumers
//! (jq pipelines, downstream agents, CI jobs) that want a single-file
//! programmatic view of the journal without parsing the rendered Markdown.
//!
//! # Output format
//!
//! ```json
//! {
//!   "schema_version": 1,
//!   "chapters": [
//!     {
//!       "chapter_id": "...",
//!       "schema_id": "journal-mcp-canonical-v1",
//!       "current_state": "closed",
//!       "opened_at": 1781502216733,
//!       "closed_at": 1781502370716,
//!       "events": [
//!         { "event_id": "...", "event_type": "section_append",
//!           "section_name": "Verified", "payload": "{...}",
//!           "created_at": 1781502264283 },
//!         ...
//!       ]
//!     },
//!     ...
//!   ]
//! }
//! ```
//!
//! Chapters are emitted in lexicographic order of `chapter_id` (the same
//! order [`FileProjection`](super::FileProjection) uses), which matches
//! chronological order for date-slug-prefixed chapter IDs.
//!
//! # Atomicity
//!
//! Writes use the `tempfile + rename` pattern (write to
//! `<path>.tmp.<pid>`, then atomic rename to the final path).  Readers
//! observe either the complete previous state or the complete new state;
//! partial writes are impossible.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process;

use serde::Serialize;

use super::{private::Sealed, JournalProjection, ProjectionError};
use crate::event_log::EventRow;
use crate::{ChapterId, ChapterReplay};

// ---------------------------------------------------------------------------
// On-disk JSON shape
// ---------------------------------------------------------------------------

/// Stable schema version of the `workspace/journal.json` output.
///
/// Bumped on any breaking change to the on-disk JSON shape.  v1 is the
/// initial v0.3.0 release.
const JSON_SCHEMA_VERSION: u32 = 1;

/// Per-chapter row in the output JSON `chapters` array.
#[derive(Debug, Serialize)]
struct ChapterDump<'a> {
    chapter_id: &'a str,
    schema_id: &'a str,
    current_state: &'a str,
    opened_at: i64,
    closed_at: Option<i64>,
    events: Vec<EventDump<'a>>,
}

/// Per-event row inside a chapter's `events` array.
#[derive(Debug, Serialize)]
struct EventDump<'a> {
    event_id: &'a str,
    event_type: &'a str,
    section_name: Option<&'a str>,
    payload: &'a str,
    created_at: i64,
}

impl<'a> From<&'a EventRow> for EventDump<'a> {
    fn from(row: &'a EventRow) -> Self {
        Self {
            event_id: &row.event_id.0,
            event_type: &row.event_type,
            section_name: row.section_name.as_deref(),
            payload: &row.payload,
            created_at: row.created_at,
        }
    }
}

/// Top-level envelope written to disk.
#[derive(Debug, Serialize)]
struct OutputEnvelope<'a> {
    schema_version: u32,
    chapters: Vec<ChapterDump<'a>>,
}

// ---------------------------------------------------------------------------
// JsonProjection
// ---------------------------------------------------------------------------

/// Machine-readable JSON dump projection.
///
/// See the module-level documentation for the output format.
pub struct JsonProjection {
    /// Filesystem path of the target JSON file (e.g. `workspace/journal.json`).
    output_path: PathBuf,

    /// In-memory map of `chapter_id` → most-recent [`ChapterReplay`].
    ///
    /// `BTreeMap` keeps chapter IDs in lexicographic order so the assembled
    /// file is deterministically ordered.  Each [`rebuild_chapter`] call
    /// inserts (or replaces) the entry for the affected chapter and then
    /// re-writes the entire file from this map.
    chapters: BTreeMap<String, ChapterReplay>,
}

impl JsonProjection {
    /// Construct a `JsonProjection` that writes to `output_path`.
    ///
    /// The file is not touched until the first [`rebuild_chapter`] call.
    /// If the file already exists, its current content is **not** read in;
    /// the in-memory map starts empty, and the file is overwritten on the
    /// first rebuild.  This matches [`FileProjection`](super::FileProjection)'s
    /// behaviour and keeps the projection stateless across process restarts
    /// (callers re-attach + rebuild from the EventLog SoT, not the
    /// FileProjection / JsonProjection output files).
    pub fn new(output_path: PathBuf) -> Self {
        Self {
            output_path,
            chapters: BTreeMap::new(),
        }
    }

    /// Return a reference to the in-memory chapter map.
    ///
    /// Used by tests to inspect internal state.
    pub fn chapters(&self) -> &BTreeMap<String, ChapterReplay> {
        &self.chapters
    }

    /// Serialize the current chapter map to a JSON byte vector.
    ///
    /// Pretty-printed (2-space indent) for human readability — the file is
    /// expected to be diffed / inspected occasionally; the size overhead
    /// compared to compact JSON is small relative to event payload size.
    fn serialize(&self) -> Result<Vec<u8>, ProjectionError> {
        let chapter_dumps: Vec<ChapterDump<'_>> = self
            .chapters
            .values()
            .map(|replay| ChapterDump {
                chapter_id: &replay.meta.chapter_id.0,
                schema_id: &replay.meta.schema_id,
                current_state: &replay.meta.current_state,
                opened_at: replay.meta.opened_at,
                closed_at: replay.meta.closed_at,
                events: replay.events.iter().map(EventDump::from).collect(),
            })
            .collect();
        let envelope = OutputEnvelope {
            schema_version: JSON_SCHEMA_VERSION,
            chapters: chapter_dumps,
        };
        Ok(serde_json::to_vec_pretty(&envelope)?)
    }

    /// Atomically write `content` to `self.output_path`.
    ///
    /// Strategy: write to a sibling temp file `<path>.tmp.<pid>`, fsync, then
    /// rename to the final path.  Rename is atomic on POSIX filesystems, so
    /// concurrent readers observe either the complete previous content or
    /// the complete new content — never a partial write.
    ///
    /// The parent directory is created with `create_dir_all` if missing
    /// (matches [`FileProjection`](super::FileProjection)'s ergonomics for
    /// fresh `workspace/` directories).
    fn write_atomic(&self, content: &[u8]) -> Result<(), ProjectionError> {
        if let Some(parent) = self.output_path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let tmp_path = self
            .output_path
            .with_extension(format!("json.tmp.{}", process::id()));
        {
            let mut tmp = fs::File::create(&tmp_path)?;
            tmp.write_all(content)?;
            tmp.sync_all()?;
        }
        fs::rename(&tmp_path, &self.output_path)?;
        Ok(())
    }
}

impl Sealed for JsonProjection {}

impl JournalProjection for JsonProjection {
    fn name(&self) -> &'static str {
        "json"
    }

    /// `JsonProjection` writes a full snapshot on every
    /// [`rebuild_chapter`](JsonProjection::rebuild_chapter) call, so
    /// `mark_dirty` is a no-op.  The next rebuild covers the dirty chapter
    /// implicitly.
    fn mark_dirty(&mut self, _id: &ChapterId) -> Result<(), ProjectionError> {
        Ok(())
    }

    /// Update the in-memory map with `replay` and re-write the entire JSON
    /// file atomically.
    fn rebuild_chapter(&mut self, replay: &ChapterReplay) -> Result<(), ProjectionError> {
        self.chapters
            .insert(replay.meta.chapter_id.0.clone(), replay.clone());
        let content = self.serialize()?;
        self.write_atomic(&content)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_log::{ChapterMeta, EventId};

    fn make_replay(chapter_id: &str, sections: &[(&str, &str)]) -> ChapterReplay {
        let events: Vec<EventRow> = sections
            .iter()
            .enumerate()
            .map(|(i, (section_name, body))| EventRow {
                event_id: EventId(format!("evt-{i}")),
                event_type: "section_append".to_owned(),
                section_name: Some((*section_name).to_owned()),
                payload: serde_json::json!({ "body": body }).to_string(),
                previous_id: None,
                created_at: 1_000_000_000 + i as i64,
            })
            .collect();
        ChapterReplay {
            meta: ChapterMeta {
                chapter_id: ChapterId(chapter_id.to_owned()),
                schema_id: "journal-mcp-canonical-v1".to_owned(),
                current_state: "closed".to_owned(),
                opened_at: 1_000_000_000,
                closed_at: Some(1_000_000_500),
            },
            events,
        }
    }

    /// T1 (property) — `new` does not touch the filesystem until the first
    /// rebuild_chapter call.
    #[test]
    fn test_new_does_not_create_file() {
        let tmp = tempfile::TempDir::new().expect("TempDir should succeed");
        let path = tmp.path().join("journal.json");
        let _proj = JsonProjection::new(path.clone());
        assert!(
            !path.exists(),
            "JsonProjection::new must not create the file"
        );
    }

    /// T2 (boundary) — `rebuild_chapter` writes a valid JSON envelope with
    /// the expected schema_version and a single chapter entry.
    #[test]
    fn test_rebuild_writes_valid_envelope() {
        let tmp = tempfile::TempDir::new().expect("TempDir should succeed");
        let path = tmp.path().join("journal.json");
        let mut proj = JsonProjection::new(path.clone());
        let replay = make_replay("chapter-1", &[("Verified", "body-alpha")]);

        proj.rebuild_chapter(&replay)
            .expect("rebuild should succeed");
        assert!(path.exists(), "file should exist after rebuild");

        let content = fs::read_to_string(&path).expect("read should succeed");
        let parsed: serde_json::Value =
            serde_json::from_str(&content).expect("output must be valid JSON");
        assert_eq!(parsed["schema_version"], 1);
        assert_eq!(parsed["chapters"].as_array().unwrap().len(), 1);
        assert_eq!(parsed["chapters"][0]["chapter_id"], "chapter-1");
        assert_eq!(parsed["chapters"][0]["events"].as_array().unwrap().len(), 1);
        assert_eq!(
            parsed["chapters"][0]["events"][0]["section_name"],
            "Verified"
        );
    }

    /// T3 (property) — `rebuild_chapter` is idempotent: calling it twice
    /// with the same replay yields the same output byte-for-byte.
    #[test]
    fn test_rebuild_idempotent() {
        let tmp = tempfile::TempDir::new().expect("TempDir should succeed");
        let path = tmp.path().join("journal.json");
        let mut proj = JsonProjection::new(path.clone());
        let replay = make_replay("chapter-2", &[("Verified", "body-beta")]);

        proj.rebuild_chapter(&replay).expect("first rebuild");
        let first = fs::read(&path).expect("read 1");
        proj.rebuild_chapter(&replay).expect("second rebuild");
        let second = fs::read(&path).expect("read 2");
        assert_eq!(
            first, second,
            "duplicate rebuilds must produce identical output"
        );
    }

    /// T4 (boundary) — multi-chapter: rebuilding two distinct chapters
    /// results in both appearing in the output (lexicographic order).
    #[test]
    fn test_multi_chapter_ordering() {
        let tmp = tempfile::TempDir::new().expect("TempDir should succeed");
        let path = tmp.path().join("journal.json");
        let mut proj = JsonProjection::new(path.clone());
        let replay_b = make_replay("chapter-B", &[("Verified", "b-body")]);
        let replay_a = make_replay("chapter-A", &[("Verified", "a-body")]);

        // Insert in non-lex order
        proj.rebuild_chapter(&replay_b).expect("rebuild B");
        proj.rebuild_chapter(&replay_a).expect("rebuild A");

        let content = fs::read_to_string(&path).expect("read");
        let parsed: serde_json::Value = serde_json::from_str(&content).expect("valid JSON");
        let chapters = parsed["chapters"].as_array().unwrap();
        assert_eq!(chapters.len(), 2);
        // BTreeMap orders by key, so "chapter-A" comes before "chapter-B".
        assert_eq!(chapters[0]["chapter_id"], "chapter-A");
        assert_eq!(chapters[1]["chapter_id"], "chapter-B");
    }

    /// T5 (property) — re-rebuilding an existing chapter replaces its
    /// entry (does not duplicate it).
    #[test]
    fn test_rebuild_replaces_existing_chapter() {
        let tmp = tempfile::TempDir::new().expect("TempDir should succeed");
        let path = tmp.path().join("journal.json");
        let mut proj = JsonProjection::new(path.clone());
        let replay_v1 = make_replay("chapter-3", &[("Verified", "v1-body")]);
        let replay_v2 = make_replay(
            "chapter-3",
            &[("Verified", "v2-body"), ("Done", "v2-done-body")],
        );

        proj.rebuild_chapter(&replay_v1).expect("rebuild v1");
        proj.rebuild_chapter(&replay_v2).expect("rebuild v2");

        let content = fs::read_to_string(&path).expect("read");
        let parsed: serde_json::Value = serde_json::from_str(&content).expect("valid JSON");
        let chapters = parsed["chapters"].as_array().unwrap();
        assert_eq!(
            chapters.len(),
            1,
            "chapter must be replaced, not duplicated"
        );
        assert_eq!(chapters[0]["events"].as_array().unwrap().len(), 2);
        assert_eq!(chapters[0]["events"][0]["section_name"], "Verified");
        assert_eq!(chapters[0]["events"][1]["section_name"], "Done");
    }

    /// T6 (boundary) — atomic write creates the parent directory if it
    /// does not yet exist (matches FileProjection's ergonomics for fresh
    /// `workspace/` directories).
    #[test]
    fn test_atomic_write_creates_parent_dir() {
        let tmp = tempfile::TempDir::new().expect("TempDir should succeed");
        let nested = tmp.path().join("nested").join("dir").join("journal.json");
        let mut proj = JsonProjection::new(nested.clone());
        let replay = make_replay("chapter-X", &[("Verified", "x-body")]);

        proj.rebuild_chapter(&replay)
            .expect("rebuild should succeed even when parent dir missing");
        assert!(nested.exists(), "nested file should be created");
    }

    /// T7 (property) — `mark_dirty` is a no-op (does not write the file).
    #[test]
    fn test_mark_dirty_no_op() {
        let tmp = tempfile::TempDir::new().expect("TempDir should succeed");
        let path = tmp.path().join("journal.json");
        let mut proj = JsonProjection::new(path.clone());

        let id = ChapterId("chapter-4".to_owned());
        proj.mark_dirty(&id).expect("mark_dirty should succeed");
        assert!(
            !path.exists(),
            "mark_dirty must not touch the filesystem; the next rebuild covers it implicitly"
        );
    }
}
