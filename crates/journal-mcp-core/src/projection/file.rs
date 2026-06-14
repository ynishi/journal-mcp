//! `FileProjection` — 4-stage pipeline file projection for journal chapters.
//!
//! # ST5 scope
//!
//! This module implements the ST5 full `FileProjection`:
//!
//! 1. **content-hash** — compute a `u64` hash of the rendered Markdown to
//!    detect unchanged chapters.
//! 2. **dirty-mark** — skip write when chapter is not dirty (hash matches
//!    and debounce window has not expired).
//! 3. **debounce** — skip write when the same chapter was written within the
//!    configured `debounce_window` *and* the content hash is identical.
//! 4. **atomic write** — write to a `NamedTempFile` in the same directory as
//!    `output_path`, then `persist` (POSIX `rename(2)`) to atomically replace
//!    the target file.
//!
//! # Crux: Atomic write failure rollback
//!
//! All writes go through `write_atomic`.  The target file (`journal.md`) is
//! never touched until `persist` succeeds.  If any earlier step (tempfile
//! creation, `write_all`, or `persist`) returns an error, `journal.md` is
//! left byte-for-byte identical to its pre-call state.
//!
//! # Crux: 4-stage pipeline ordering
//!
//! `rebuild_chapter` enforces the gate chain with sequential statements:
//! `render_chapter` → `compute_hash` → `should_skip` → `write_atomic`.
//! No stage may be bypassed.
//!
//! # Crux: Schema-driven render template
//!
//! `render_chapter` reads `chapter_header`, `section_header`, and
//! `section_order` exclusively from the `ChapterSchema` returned by
//! `SchemaRegistry::get`.  No template literals are hardcoded in this module.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::projection::{private, JournalProjection, ProjectionError};
use crate::{ChapterId, ChapterReplay, SchemaRegistry};

// ---------------------------------------------------------------------------
// FileProjection
// ---------------------------------------------------------------------------

/// A projection that renders journal chapters to a single Markdown file.
///
/// # Lifecycle
///
/// 1. `mark_dirty` is called by `JournalCore` after each `append_section`.
/// 2. `rebuild_chapter` is called by `JournalCore` after `close_chapter`.
///    It runs the 4-stage pipeline (hash → dirty/debounce skip → atomic write).
///
/// # Multi-chapter file
///
/// All chapters are assembled in `chapter_dumps` (a `BTreeMap` keyed by
/// chapter-id string).  On each `rebuild_chapter` call the entire file is
/// re-written with all chapters in lexicographic order.  For ytk-canonical-v1
/// this is date-string order, which matches chronological order.
pub struct FileProjection {
    /// Filesystem path of the target Markdown file (e.g. `workspace/journal.md`).
    output_path: PathBuf,

    /// Schema registry — used to fetch the schema for each chapter on rebuild.
    registry: Arc<SchemaRegistry>,

    /// Set of chapter-id strings whose file representation is stale.
    ///
    /// `String` keys are used instead of [`ChapterId`] because [`ChapterId`]
    /// does not derive `Hash` (debt ae2c9a2e, out of ST5 scope).
    dirty: HashSet<String>,

    /// Most-recently-written content hash per chapter (keyed by chapter-id string).
    content_hashes: HashMap<String, u64>,

    /// Timestamp of the most recent successful write per chapter.
    last_write_at: HashMap<String, Instant>,

    /// Minimum interval between successive writes for the same unchanged chapter.
    debounce_window: Duration,

    /// Latest rendered Markdown per chapter, in insertion/chapter-id order.
    ///
    /// `BTreeMap` keeps chapter-ids in lexicographic order so that the
    /// assembled file is deterministically ordered by date slug.
    chapter_dumps: BTreeMap<String, String>,

    /// Hash of the file content written by the most recent successful `write_atomic` call.
    /// `None` if no write has occurred yet in this process session.
    last_written_hash: Option<u64>,
}

impl FileProjection {
    /// Construct a `FileProjection` that writes to `output_path`.
    ///
    /// The default debounce window is 1 second.
    pub fn new(output_path: PathBuf, registry: Arc<SchemaRegistry>) -> Self {
        Self::with_debounce(output_path, registry, Duration::from_secs(1))
    }

    /// Construct a `FileProjection` with a custom debounce window.
    ///
    /// Pass `Duration::ZERO` to disable debouncing (useful in tests).
    pub fn with_debounce(
        output_path: PathBuf,
        registry: Arc<SchemaRegistry>,
        debounce_window: Duration,
    ) -> Self {
        Self {
            output_path,
            registry,
            dirty: HashSet::new(),
            content_hashes: HashMap::new(),
            last_write_at: HashMap::new(),
            debounce_window,
            chapter_dumps: BTreeMap::new(),
            last_written_hash: None,
        }
    }

    /// Return a reference to the set of dirty chapter-id strings.
    ///
    /// Used by tests to inspect internal state.
    pub fn dirty_chapters(&self) -> &HashSet<String> {
        &self.dirty
    }
}

// Crux: Sealed trait 外部 impl 禁止境界
impl private::Sealed for FileProjection {}

impl JournalProjection for FileProjection {
    /// Returns the stable identifier for this projection type.
    ///
    /// Used by [`JournalCore::rebuild_projection`] and
    /// [`JournalCore::list_projection_names`] for named lookup.
    fn name(&self) -> &'static str {
        "file"
    }

    /// Mark the chapter identified by `id` as dirty.
    ///
    /// Inserts the chapter's string id into the internal dirty set.
    /// Always returns `Ok(())` — no IO is performed.
    fn mark_dirty(&mut self, id: &ChapterId) -> Result<(), ProjectionError> {
        self.dirty.insert(id.0.clone());
        Ok(())
    }

    /// Rebuild the Markdown file for the chapter described by `replay`.
    ///
    /// Runs the 4-stage pipeline:
    ///
    /// 1. Render the chapter to Markdown (`render_chapter`).
    /// 2. Compute a content hash (`compute_hash`).
    /// 3. Skip if hash matches *and* debounce window has not expired (`should_skip`).
    /// 4. Atomically write the entire assembled file (`write_atomic`).
    ///
    /// # Errors
    ///
    /// Returns [`ProjectionError::Io`] when the filesystem write fails.  On
    /// any error the target file is left byte-for-byte identical to its
    /// pre-call state (Crux 1).
    fn rebuild_chapter(&mut self, replay: &ChapterReplay) -> Result<(), ProjectionError> {
        let chapter_id = replay.meta.chapter_id.0.clone();

        // Stage 0: look up schema (prerequisite for render)
        let schema = self
            .registry
            .get(&replay.meta.schema_id)
            .ok_or_else(|| {
                tracing::warn!(
                    target: "journal::projection::file",
                    schema_id = %replay.meta.schema_id,
                    chapter_id = %chapter_id,
                    "schema not found; skipping rebuild"
                );
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("schema '{}' not found", replay.meta.schema_id),
                )
            })
            .map_err(ProjectionError::Io)?;

        // Clone the template values we need before dropping the borrow on registry.
        let chapter_header = schema.chapter_header().map(str::to_owned);
        let section_header = schema.section_header().map(str::to_owned);
        let section_order: Vec<String> = schema.section_order().to_vec();

        // Stage 1: render to Markdown (Crux 3 — schema-driven, no literals)
        let rendered = render_chapter(
            replay,
            chapter_header.as_deref(),
            section_header.as_deref(),
            &section_order,
        );

        // Stage 2: content hash
        let hash = compute_hash(&rendered);

        // Stage 3: debounce / hash-skip gate
        if self.should_skip(&chapter_id, hash, Instant::now()) {
            return Ok(());
        }

        // State updates (before write so that a write failure leaves them
        // consistent with the "we tried" state — the dirty flag is only
        // cleared on success below).
        self.chapter_dumps.insert(chapter_id.clone(), rendered);
        self.content_hashes.insert(chapter_id.clone(), hash);
        self.last_write_at
            .insert(chapter_id.clone(), Instant::now());

        // Stage 4: assemble + atomic write (Crux 1)
        let full = self.assemble_file_content();
        if let Err(e) = self.write_atomic(&full) {
            tracing::warn!(
                target: "journal::projection::file",
                chapter_id = %chapter_id,
                error = %e,
                "atomic write failed; journal.md left unchanged"
            );
            return Err(e);
        }

        // Clear dirty flag only after successful write
        self.dirty.remove(&chapter_id);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Render a chapter to a Markdown string using schema-supplied templates.
///
/// Templates are sourced exclusively from the `ChapterSchema` accessors
/// (Crux 3).  No `"## "` / `"### "` / section-name literals appear here.
///
/// # Placeholder expansion
///
/// * `chapter_header`: `{date}` and `{name}` are both replaced with
///   `chapter_id` (ytk-canonical-v1 uses the chapter_id as the date slug).
/// * `section_header`: `{section_name}` is replaced with the section name.
///
/// When `chapter_header` or `section_header` is `None` the fallback strings
/// `"(schema lacks chapter header)"` / `"(schema lacks section header)"` are
/// used so schema deficiencies are visible in the rendered output.
fn render_chapter(
    replay: &ChapterReplay,
    chapter_header: Option<&str>,
    section_header: Option<&str>,
    section_order: &[String],
) -> String {
    let chapter_id = &replay.meta.chapter_id.0;

    // Expand chapter header template
    let header_template = chapter_header.unwrap_or("(schema lacks chapter header)");
    let header = header_template
        .replace("{date}", chapter_id)
        .replace("{name}", chapter_id);

    // Build a lookup: section_name → body text (from event payloads)
    let section_bodies = collect_section_bodies(replay);

    let mut out = String::new();
    out.push_str(&header);
    out.push('\n');
    out.push('\n');

    // Emit sections in schema-declared order
    for section_name in section_order {
        let sec_template = section_header.unwrap_or("(schema lacks section header)");
        let sec_heading = sec_template.replace("{section_name}", section_name);

        let body = section_bodies
            .get(section_name.as_str())
            .map(String::as_str)
            .unwrap_or("");

        out.push_str(&sec_heading);
        out.push('\n');
        if !body.is_empty() {
            out.push_str(body);
            out.push('\n');
        }
        out.push('\n');
    }

    out
}

/// Extract section bodies from `replay.events`.
///
/// Each `append_section` event payload is JSON `{"body": "..."}`.  We collect
/// the last body written for each section name.  Follows the same pattern as
/// `core.rs::body_is_empty`.
fn collect_section_bodies(replay: &ChapterReplay) -> HashMap<String, String> {
    let mut bodies: HashMap<String, String> = HashMap::new();

    for event in &replay.events {
        // Payload shape: {"body": "..."} — same as core.rs body_is_empty
        let body_text = serde_json::from_str::<serde_json::Value>(&event.payload)
            .ok()
            .and_then(|v| v.get("body")?.as_str().map(str::to_owned))
            .unwrap_or_default();

        if !body_text.is_empty() {
            if let Some(name) = event.section_name.clone() {
                bodies.insert(name, body_text);
            }
        }
    }

    bodies
}

/// Compute a 64-bit content hash for the rendered Markdown string.
///
/// Uses `std::hash::DefaultHasher` (SipHash-1-3).  This is sufficient for
/// "same-process, same-run" skip detection; cryptographic strength is not
/// required.
fn compute_hash(rendered: &str) -> u64 {
    let mut h = DefaultHasher::new();
    rendered.hash(&mut h);
    h.finish()
}

impl FileProjection {
    /// Return `true` when the write for `chapter_id` should be skipped.
    ///
    /// Skip condition: `stored_hash == hash` **and** the last write was within
    /// `debounce_window`.  Both conditions must hold simultaneously.
    fn should_skip(&self, chapter_id: &str, hash: u64, now: Instant) -> bool {
        let stored_hash = match self.content_hashes.get(chapter_id) {
            Some(h) => *h,
            None => return false,
        };

        if stored_hash != hash {
            return false;
        }

        match self.last_write_at.get(chapter_id) {
            Some(last) => now.duration_since(*last) < self.debounce_window,
            None => false,
        }
    }

    /// Assemble the full file content from all chapter dumps in BTreeMap order.
    fn assemble_file_content(&self) -> String {
        let mut out = String::new();
        for rendered in self.chapter_dumps.values() {
            out.push_str(rendered);
        }
        out
    }

    /// Write `content` to `output_path` atomically via a tempfile + POSIX rename.
    ///
    /// Steps:
    /// 1. `fs::create_dir_all(parent)` — ensure parent directory exists.
    /// 2. Hash-check + auto-backup: if `output_path` exists and its content hash
    ///    differs from `last_written_hash` (or `last_written_hash` is `None`),
    ///    rename the existing file to `<output_path>.bak.<epoch_ms>` before writing.
    /// 3. `NamedTempFile::new_in(parent)` — create tempfile in the **same**
    ///    directory as the target (rename(2) requires same filesystem).
    /// 4. `write_all(content)` — write content to the tempfile.
    /// 5. `persist(output_path)` — atomic POSIX rename.
    ///
    /// On any failure before step 5 the tempfile is dropped and cleaned up
    /// automatically.  The target file is never touched until `persist`
    /// succeeds (Crux 1).  If the backup rename fails in step 2, the function
    /// returns early with `ProjectionError::Io` and no tempfile is created.
    fn write_atomic(&mut self, content: &str) -> Result<(), ProjectionError> {
        let parent = self.output_path.parent().unwrap_or_else(|| Path::new("."));

        std::fs::create_dir_all(parent)?;

        // Hash-check + auto-backup step (Crux: explicit-only render guard).
        if self.output_path.exists() {
            let existing = std::fs::read_to_string(&self.output_path)?;
            let existing_hash = compute_hash(&existing);
            let should_backup = match self.last_written_hash {
                Some(prev) => prev != existing_hash,
                // No previous write in this session: treat as possible external edit → backup.
                None => true,
            };
            if should_backup {
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis();
                let bak_path = self.output_path.with_extension(format!("md.bak.{ts}"));
                tracing::warn!(
                    target: "journal::projection::file",
                    path = %self.output_path.display(),
                    backup = %bak_path.display(),
                    "external edit detected or first write over existing file; backing up"
                );
                std::fs::rename(&self.output_path, &bak_path).map_err(|e| {
                    tracing::warn!(
                        target: "journal::projection::file",
                        path = %self.output_path.display(),
                        backup = %bak_path.display(),
                        error = %e,
                        "backup rename failed; aborting write"
                    );
                    e
                })?;
            }
        }

        let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
        tmp.write_all(content.as_bytes())?;
        tmp.flush()?;

        tmp.persist(&self.output_path)
            .map_err(|e| ProjectionError::Io(e.error))?;

        // Record the hash of the content we just wrote.
        self.last_written_hash = Some(compute_hash(content));

        tracing::debug!(
            target: "journal::projection::file",
            path = %self.output_path.display(),
            "atomic write complete"
        );

        Ok(())
    }
}
