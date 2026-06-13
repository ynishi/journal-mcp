//! Integration tests for `FileProjection` (ST4).
//!
//! Two property categories:
//! - T1 (property): `mark_dirty` inserts the chapter id into the dirty set.
//! - T2 (edge): `rebuild_chapter` removes the chapter id from the dirty set,
//!   and calling it for a chapter that was never marked dirty is a no-op.

use journal::{ChapterId, ChapterMeta, ChapterReplay, EventRow, FileProjection, JournalProjection};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a minimal `ChapterReplay` for the given chapter id.
///
/// `current_state` is set to `"closed"` as expected after `close_chapter`.
fn make_replay(id: &str) -> ChapterReplay {
    ChapterReplay {
        meta: ChapterMeta {
            chapter_id: ChapterId(id.to_owned()),
            schema_id: "ytk-canonical-v1".to_owned(),
            current_state: "closed".to_owned(),
            opened_at: 0,
            closed_at: Some(0),
        },
        events: Vec::<EventRow>::new(),
    }
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
    let mut fp = FileProjection::new();

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
/// - Calling `rebuild_chapter` for a never-marked chapter is a no-op (no
///   panic, dirty set stays empty).
#[test]
fn test_rebuild_chapter_clears_dirty() {
    let mut fp = FileProjection::new();

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
    let mut fp = FileProjection::new();

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
