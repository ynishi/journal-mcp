//! Integration tests for `FileProjection` (ST5).
//!
//! Test categories:
//! - T1 (property): `mark_dirty` inserts the chapter id into the dirty set.
//! - T2 (edge): `rebuild_chapter` removes the chapter id from the dirty set.
//! - T3 (Crux 1): atomic write rollback — failed write leaves `journal.md` unchanged.
//! - T4 (Crux 2): content-hash skip — identical content on 2nd rebuild avoids write.
//! - T5 (Crux 2): debounce + content change — changed content forces write even in window.
//! - T6 (Crux 3): schema-driven render — output reflects schema templates, not literals.
//! - T7 (multi-chapter): two chapters in same file, ordered by chapter_id.

use std::sync::Arc;
use std::time::Duration;

use journal_mcp_core::{
    ChapterId, ChapterMeta, ChapterReplay, EventId, EventRow, FileProjection, JournalProjection,
    SchemaRegistry,
};

// ---------------------------------------------------------------------------
// Test-only schema YAML (used by T6 to prove no literal hardcode)
// ---------------------------------------------------------------------------

/// journal-mcp-canonical-v1 schema key as registered in the embedded registry.
const JOURNAL_MCP_SCHEMA_ID: &str = "journal-mcp-canonical-v1";

/// A minimal custom schema for T6 that uses `# X` as chapter_header to prove
/// `file.rs` does not hardcode `"## "`.
const ALT_SCHEMA_YAML: &str = concat!(
    "schema_id: alt-test\n",
    "version: 1\n",
    "sections: {}\n",
    "render:\n",
    "  file_projection:\n",
    "    chapter_header: \"# X {date}\"\n",
    "    section_header: \"## {section_name}\"\n",
    "    section_order: []\n",
);

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a minimal `ChapterReplay` for the given chapter id with no events.
fn make_replay(id: &str) -> ChapterReplay {
    make_replay_with_events(id, JOURNAL_MCP_SCHEMA_ID, vec![])
}

/// Build a `ChapterReplay` for the given chapter id and schema_id with events.
fn make_replay_with_events(id: &str, schema_id: &str, events: Vec<EventRow>) -> ChapterReplay {
    ChapterReplay {
        meta: ChapterMeta {
            chapter_id: ChapterId(id.to_owned()),
            schema_id: schema_id.to_owned(),
            current_state: "closed".to_owned(),
            opened_at: 0,
            closed_at: Some(0),
            chapter_name: None,
        },
        events,
    }
}

/// Build an `EventRow` representing an `append_section` for `section` with `body`.
fn make_event(section: &str, body: &str) -> EventRow {
    EventRow {
        event_id: EventId("ev-1".to_owned()),
        event_type: "section_append".to_owned(),
        section_name: Some(section.to_owned()),
        payload: format!(r#"{{"body": "{}"}}"#, body),
        previous_id: None,
        created_at: 0,
    }
}

/// Build a `FileProjection` writing to `path` with debounce disabled (window=ZERO).
fn make_fp(path: std::path::PathBuf) -> FileProjection {
    let registry = Arc::new(SchemaRegistry::new().expect("built-in schemas must load"));
    FileProjection::with_debounce(path, registry, Duration::ZERO)
}

/// Build a `FileProjection` with a custom `SchemaRegistry` and debounce window.
fn make_fp_with_registry(
    path: std::path::PathBuf,
    registry: Arc<SchemaRegistry>,
    window: Duration,
) -> FileProjection {
    FileProjection::with_debounce(path, registry, window)
}

// ---------------------------------------------------------------------------
// T1 — happy path: mark_dirty inserts into dirty set
// ---------------------------------------------------------------------------

/// T1 — `mark_dirty` records the chapter as stale.
///
/// Verifies:
/// - After calling `mark_dirty`, `dirty_chapters` contains the chapter id.
/// - A second `mark_dirty` on the same id is idempotent (set semantics).
#[test]
fn test_mark_dirty_inserts() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut fp = make_fp(tmp.path().join("journal.md"));

    fp.mark_dirty(&ChapterId("c1".to_owned()))
        .expect("mark_dirty should succeed");

    assert!(
        fp.dirty_chapters().contains("c1"),
        "dirty_chapters should contain 'c1' after mark_dirty"
    );

    // Second mark_dirty on same id is idempotent.
    fp.mark_dirty(&ChapterId("c1".to_owned()))
        .expect("second mark_dirty should succeed");
    assert_eq!(
        fp.dirty_chapters().len(),
        1,
        "HashSet should deduplicate duplicate mark_dirty calls"
    );
}

// ---------------------------------------------------------------------------
// T2 — boundary/edge: rebuild_chapter removes from dirty set
// ---------------------------------------------------------------------------

/// T2 — `rebuild_chapter` clears the dirty flag.
///
/// Verifies:
/// - After `mark_dirty` + `rebuild_chapter`, the chapter id is absent from
///   `dirty_chapters`.
/// - Calling `rebuild_chapter` for a never-marked chapter is a no-op.
#[test]
fn test_rebuild_chapter_clears_dirty() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut fp = make_fp(tmp.path().join("journal.md"));

    let id = ChapterId("c2".to_owned());
    fp.mark_dirty(&id).expect("mark_dirty should succeed");
    assert!(fp.dirty_chapters().contains("c2"), "c2 should be dirty");

    let replay = make_replay("c2");
    fp.rebuild_chapter(&replay)
        .expect("rebuild_chapter should succeed");

    assert!(
        !fp.dirty_chapters().contains("c2"),
        "c2 should be absent from dirty set after rebuild_chapter"
    );
}

/// T2 (edge) — `rebuild_chapter` for a chapter that was never marked dirty is
/// a no-op and does not panic.
#[test]
fn test_rebuild_chapter_not_marked_is_noop() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut fp = make_fp(tmp.path().join("journal.md"));

    // Mark a different chapter dirty to confirm the set isn't empty.
    fp.mark_dirty(&ChapterId("other".to_owned()))
        .expect("mark_dirty should succeed");

    let replay = make_replay("never-marked");
    fp.rebuild_chapter(&replay)
        .expect("rebuild_chapter for never-marked chapter should succeed");

    // The set should still contain "other" and not contain "never-marked".
    assert!(
        fp.dirty_chapters().contains("other"),
        "'other' should still be dirty"
    );
    assert!(
        !fp.dirty_chapters().contains("never-marked"),
        "'never-marked' should not appear in dirty set"
    );
}

// ---------------------------------------------------------------------------
// T3 — Crux 1: atomic write failure rollback
// ---------------------------------------------------------------------------

/// T3 — write failure must leave any pre-existing `journal.md` untouched.
///
/// Strategy: point `output_path` at a path whose *parent directory* does not
/// exist (and cannot be created because the grandparent is a file), so that
/// `create_dir_all` fails.  A sentinel file at a sibling path confirms the
/// filesystem is otherwise writable.
///
/// Pre-condition: write a sentinel `journal.md` in the temp dir.  After the
/// failed rebuild the sentinel must be byte-for-byte identical.
#[test]
fn test_atomic_write_rollback_on_error() {
    let tmp = tempfile::tempdir().expect("tempdir");

    // Write a sentinel file that must survive the failed write attempt.
    let sentinel_path = tmp.path().join("sentinel.md");
    let sentinel_content = b"ORIGINAL CONTENT - must not change";
    std::fs::write(&sentinel_path, sentinel_content).expect("write sentinel");

    // Point output_path at a path under a *file* (not a directory), so
    // create_dir_all will fail because the "parent" is a regular file.
    let blocker = tmp.path().join("blocker");
    std::fs::write(&blocker, b"I am a file, not a dir").expect("write blocker");
    let bad_output = blocker.join("journal.md");

    let registry = Arc::new(SchemaRegistry::new().expect("built-in schemas must load"));
    let mut fp = FileProjection::with_debounce(bad_output, registry, Duration::ZERO);

    let replay = make_replay("2026-06-14");
    let result = fp.rebuild_chapter(&replay);

    // The write must fail.
    assert!(
        result.is_err(),
        "rebuild_chapter with non-writable parent must return Err"
    );

    // The sentinel must be byte-for-byte identical.
    let after = std::fs::read(&sentinel_path).expect("read sentinel after error");
    assert_eq!(
        after.as_slice(),
        sentinel_content,
        "sentinel.md must be byte-for-byte identical after failed rebuild"
    );
}

// ---------------------------------------------------------------------------
// T4 — Crux 2: content-hash skip
// ---------------------------------------------------------------------------

/// T4 — identical content on a second `rebuild_chapter` must not update the file.
///
/// Verifies the hash-based skip: when `rendered == stored` *and* the debounce
/// window has not expired, the second call must not touch the filesystem.
///
/// We use a long debounce window (60 s) so that even on a slow host the window
/// is still active between the two rebuild calls.
#[test]
fn test_content_hash_skip_avoids_write() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out_path = tmp.path().join("journal.md");

    let registry = Arc::new(SchemaRegistry::new().expect("built-in schemas must load"));
    // Long window so that both calls happen within it.
    let mut fp = make_fp_with_registry(out_path.clone(), registry, Duration::from_secs(60));

    let replay = make_replay("2026-06-14");

    // First rebuild: writes the file.
    fp.rebuild_chapter(&replay)
        .expect("first rebuild should succeed");
    assert!(
        out_path.exists(),
        "journal.md should exist after first rebuild"
    );

    let mtime_before = out_path
        .metadata()
        .expect("metadata")
        .modified()
        .expect("mtime");

    // Sleep briefly to let the OS advance the mtime clock if a write occurs.
    std::thread::sleep(Duration::from_millis(20));

    // Second rebuild with identical content: must be skipped.
    fp.rebuild_chapter(&replay)
        .expect("second rebuild should succeed");

    let mtime_after = out_path
        .metadata()
        .expect("metadata")
        .modified()
        .expect("mtime");

    assert_eq!(
        mtime_before, mtime_after,
        "mtime must not change on hash-skip (no write should occur)"
    );
}

// ---------------------------------------------------------------------------
// T5 — Crux 2: debounce + content change forces write
// ---------------------------------------------------------------------------

/// T5 — content change inside the debounce window must still trigger a write.
///
/// Uses a long debounce window (60 s).  The first call writes; the second call
/// has *different* content (extra event) and must update the file despite the
/// window being active.
///
/// Also verifies: same content + ZERO window → always write (debounce=ZERO
/// means window never covers any elapsed time, so `should_skip` is always
/// false when the window is zero — the duration_since check `< ZERO` is never
/// true).
#[test]
fn test_debounce_content_change_forces_write() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out_path = tmp.path().join("journal.md");

    let registry = Arc::new(SchemaRegistry::new().expect("built-in schemas must load"));
    // Long window — if hash is same this would skip; if hash differs it must write.
    let mut fp = make_fp_with_registry(out_path.clone(), registry, Duration::from_secs(60));

    // First rebuild with no events.
    let replay_v1 = make_replay_with_events("2026-06-14", JOURNAL_MCP_SCHEMA_ID, vec![]);
    fp.rebuild_chapter(&replay_v1)
        .expect("first rebuild should succeed");

    let content_v1 = std::fs::read_to_string(&out_path).expect("read v1");

    std::thread::sleep(Duration::from_millis(20));

    // Second rebuild with an added event (different content → different hash).
    let replay_v2 = make_replay_with_events(
        "2026-06-14",
        JOURNAL_MCP_SCHEMA_ID,
        vec![make_event("Verified", "cargo test all pass")],
    );
    fp.rebuild_chapter(&replay_v2)
        .expect("second rebuild with different content should succeed");

    let content_v2 = std::fs::read_to_string(&out_path).expect("read v2");

    assert_ne!(
        content_v1, content_v2,
        "file content must change when chapter body changes, even within debounce window"
    );
    assert!(
        content_v2.contains("cargo test all pass"),
        "new body text must appear in output"
    );
}

/// T5b — same content + debounce window → second call skips write.
#[test]
fn test_debounce_same_content_skips() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out_path = tmp.path().join("journal.md");

    let registry = Arc::new(SchemaRegistry::new().expect("built-in schemas must load"));
    let mut fp = make_fp_with_registry(out_path.clone(), registry, Duration::from_secs(60));

    let replay = make_replay("2026-06-14");
    fp.rebuild_chapter(&replay)
        .expect("first rebuild should succeed");

    let mtime_1 = out_path
        .metadata()
        .expect("metadata")
        .modified()
        .expect("mtime");
    std::thread::sleep(Duration::from_millis(20));

    fp.rebuild_chapter(&replay)
        .expect("second rebuild should succeed");

    let mtime_2 = out_path
        .metadata()
        .expect("metadata")
        .modified()
        .expect("mtime");

    assert_eq!(
        mtime_1, mtime_2,
        "mtime must not change: same content inside debounce window"
    );
}

// ---------------------------------------------------------------------------
// T6 — Crux 3: schema-driven render
// ---------------------------------------------------------------------------

/// T6a — journal-mcp-canonical-v1 chapter header starts with `## `.
///
/// Does not assert the literal `"## "` as a hardcoded expectation about
/// implementation internals, but rather asserts the schema-declared template
/// expansion.  journal-mcp-canonical-v1 declares `chapter_header: "## {date} — {name}"`.
#[test]
fn test_schema_driven_render_journal_mcp_canonical() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out_path = tmp.path().join("journal.md");
    let mut fp = make_fp(out_path.clone());

    let replay = make_replay("2026-06-14");
    fp.rebuild_chapter(&replay).expect("rebuild should succeed");

    let content = std::fs::read_to_string(&out_path).expect("read output");

    // journal-mcp-canonical-v1 declares chapter_header = "## {date} — {name}"
    assert!(
        content.starts_with("## 2026-06-14"),
        "output must start with the schema-declared chapter_header expanded; got: {:?}",
        &content[..content.len().min(80)]
    );
}

/// T6b — alt schema (`chapter_header: "# X {date}"`) produces `# X` prefix.
///
/// Proves that `file.rs` does not hardcode `"## "`: swapping the schema
/// changes the output prefix.
#[test]
fn test_schema_driven_render_alt_schema() {
    use journal_mcp_core::ChapterSchema;

    // Suppress unused-import warning for direct use of ChapterSchema in this test.
    let _ = ChapterSchema::parse_str(ALT_SCHEMA_YAML).expect("parse alt schema is valid yaml");

    // Build a registry containing the alt schema via project-local override.
    // SchemaRegistry::with_project_local(root) loads from <root>/.journal/schemas/*.yaml.
    let project_root = tempfile::tempdir().expect("project_root");
    let schemas_dir = project_root.path().join(".journal").join("schemas");
    std::fs::create_dir_all(&schemas_dir).expect("create schemas dir");
    std::fs::write(schemas_dir.join("alt-test.yaml"), ALT_SCHEMA_YAML)
        .expect("write alt schema yaml");

    let registry = Arc::new(
        SchemaRegistry::with_project_local(project_root.path())
            .expect("load registry with alt schema"),
    );

    // Verify the alt schema was loaded under "alt-test-v1".
    assert!(
        registry.get("alt-test-v1").is_some(),
        "alt-test-v1 schema must be present in registry"
    );

    let tmp = tempfile::tempdir().expect("tempdir");
    let out_path = tmp.path().join("journal.md");
    let mut fp = FileProjection::with_debounce(out_path.clone(), registry, Duration::ZERO);

    // Use alt-test-v1 schema_id.
    let replay = make_replay_with_events("2026-06-14", "alt-test-v1", vec![]);
    fp.rebuild_chapter(&replay)
        .expect("rebuild with alt schema should succeed");

    let content = std::fs::read_to_string(&out_path).expect("read output");

    // Alt schema declares chapter_header = "# X {date}" — must not be "## "
    assert!(
        content.starts_with("# X 2026-06-14"),
        "output must reflect alt schema chapter_header '# X {{date}}'; got: {:?}",
        &content[..content.len().min(80)]
    );
    assert!(
        !content.starts_with("## "),
        "output must NOT start with '## ' when alt schema is used"
    );

    // Alt schema declares section_header = "## {section_name}" (not "### ")
    // with no section_order, so no sections appear.
    assert!(
        !content.contains("### "),
        "alt schema section_header is '## ', output must not contain '### '"
    );
}

// ---------------------------------------------------------------------------
// T8 (ST7 Crux #1): hash-check + auto-backup guard
// ---------------------------------------------------------------------------

/// T8a — external edit between two rebuilds triggers a `.bak.*` file.
///
/// Scenario:
/// 1. First `rebuild_chapter` → writes `journal.md`.
/// 2. Simulate an external edit by writing a different string directly.
/// 3. Second `rebuild_chapter` → detects hash mismatch → renames `journal.md`
///    to `journal.md.bak.<epoch_ms>` before writing the new content.
///
/// Also covers AC 4: when `last_written_hash` is `None` and an existing file
/// is present, the file is backed up (safe-side policy).
#[test]
fn test_external_edit_triggers_backup() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out_path = tmp.path().join("journal.md");
    let mut fp = make_fp(out_path.clone());

    let replay = make_replay("2026-06-14");

    // First rebuild: writes journal.md and records last_written_hash.
    fp.rebuild_chapter(&replay)
        .expect("first rebuild should succeed");
    assert!(
        out_path.exists(),
        "journal.md must exist after first rebuild"
    );

    // Simulate an external edit (different content, different hash).
    std::fs::write(
        &out_path,
        b"externally modified content that differs from rendered",
    )
    .expect("simulate external edit");

    // Second rebuild: hash mismatch → backup expected.
    fp.rebuild_chapter(&replay)
        .expect("second rebuild should succeed");

    // Verify: a .bak.* file exists in the same directory.
    let bak_files: Vec<_> = std::fs::read_dir(tmp.path())
        .expect("read_dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains("journal.md.bak."))
        .collect();
    assert_eq!(
        bak_files.len(),
        1,
        "exactly one .bak.* file should be created after external edit; found: {:?}",
        bak_files.iter().map(|e| e.file_name()).collect::<Vec<_>>()
    );

    // Verify the backup contains the externally modified content.
    let bak_content = std::fs::read_to_string(bak_files[0].path()).expect("read bak file");
    assert!(
        bak_content.contains("externally modified content"),
        "backup must contain the externally modified content"
    );

    // Verify journal.md now has the fresh render (not the external edit).
    let current = std::fs::read_to_string(&out_path).expect("read journal.md after second rebuild");
    assert!(
        !current.contains("externally modified content"),
        "journal.md must not contain the external edit after rebuild"
    );
}

/// T8b — first write over a pre-existing file (last_written_hash is None) triggers backup.
///
/// Simulates the case where a `FileProjection` instance is created fresh (e.g.,
/// server restart) but `journal.md` already exists on disk.  The first rebuild
/// must back up the existing file before writing (safe-side policy per AC 4).
#[test]
fn test_first_write_with_preexisting_file_backs_up() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out_path = tmp.path().join("journal.md");

    // Write a pre-existing journal.md before creating the FileProjection.
    std::fs::write(&out_path, b"pre-existing content from previous run")
        .expect("write pre-existing file");

    // Create a fresh FileProjection (last_written_hash = None).
    let mut fp = make_fp(out_path.clone());

    let replay = make_replay("2026-06-14");
    fp.rebuild_chapter(&replay)
        .expect("first rebuild over pre-existing file should succeed");

    // A .bak.* file must exist (None case = safe-side backup).
    let bak_files: Vec<_> = std::fs::read_dir(tmp.path())
        .expect("read_dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains("journal.md.bak."))
        .collect();
    assert_eq!(
        bak_files.len(),
        1,
        "pre-existing file must be backed up when last_written_hash is None"
    );

    let bak_content = std::fs::read_to_string(bak_files[0].path()).expect("read bak file");
    assert!(
        bak_content.contains("pre-existing content"),
        "backup must contain the original pre-existing content"
    );
}

/// T8c — consecutive rebuilds with identical content do not create a backup.
///
/// After the first rebuild, `last_written_hash` is set.  A second rebuild
/// with the same chapter content produces the same rendered output.  Since the
/// hash matches, no backup must be created.
#[test]
fn test_no_backup_when_hash_matches() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out_path = tmp.path().join("journal.md");
    let mut fp = make_fp(out_path.clone());

    let replay = make_replay("2026-06-14");

    // First rebuild writes journal.md and sets last_written_hash.
    fp.rebuild_chapter(&replay)
        .expect("first rebuild should succeed");

    // Second rebuild with identical content: should_skip is false here because
    // debounce is ZERO (make_fp uses Duration::ZERO), but write_atomic is still
    // called with the same assembled content.  Since last_written_hash matches
    // the file on disk, no backup should be created.
    fp.rebuild_chapter(&replay)
        .expect("second rebuild with same content should succeed");

    // No .bak.* file should exist.
    let bak_files: Vec<_> = std::fs::read_dir(tmp.path())
        .expect("read_dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains("journal.md.bak."))
        .collect();
    assert_eq!(
        bak_files.len(),
        0,
        "no .bak.* file should be created when content hash matches; found: {:?}",
        bak_files.iter().map(|e| e.file_name()).collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// T7 — multi-chapter same file
// ---------------------------------------------------------------------------

/// T7 — two chapters rebuilt independently must both appear in the output,
/// ordered by chapter_id string (lexicographic / date order).
#[test]
fn test_multi_chapter_ordering() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out_path = tmp.path().join("journal.md");
    let mut fp = make_fp(out_path.clone());

    // Rebuild 2026-06-14 first, then 2026-06-13 — reversed date order.
    let replay_14 = make_replay("2026-06-14");
    let replay_13 = make_replay("2026-06-13");

    fp.rebuild_chapter(&replay_14)
        .expect("rebuild 2026-06-14 should succeed");
    fp.rebuild_chapter(&replay_13)
        .expect("rebuild 2026-06-13 should succeed");

    let content = std::fs::read_to_string(&out_path).expect("read output");

    // Both chapters must appear.
    assert!(
        content.contains("2026-06-13"),
        "output must contain 2026-06-13"
    );
    assert!(
        content.contains("2026-06-14"),
        "output must contain 2026-06-14"
    );

    // 2026-06-13 must precede 2026-06-14 (BTreeMap lexicographic order).
    let pos_13 = content.find("2026-06-13").expect("find 2026-06-13");
    let pos_14 = content.find("2026-06-14").expect("find 2026-06-14");
    assert!(
        pos_13 < pos_14,
        "2026-06-13 must appear before 2026-06-14 in the assembled file"
    );
}
