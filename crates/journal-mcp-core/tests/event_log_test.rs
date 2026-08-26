//! Integration tests for `EventLog`.
//!
//! Three property categories:
//! - T1 happy path (append + replay)
//! - T2 immutability / boundary (trigger abort on UPDATE/DELETE)
//! - T3 error path (chapter_meta state transition verification)

use std::thread;
use std::time::Duration;

use journal_mcp_core::{ChapterId, EventLog};

/// T1 — Happy path: open a chapter, append three sections, close, then replay.
///
/// Verifies that all five events are returned in order, chapter_meta reflects
/// the closed state, and closed_at is set.
#[test]
fn test_happy_path_append_and_replay() {
    // SAFETY: TempDir is kept alive for the duration of the test.
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join(".journal.db");

    let mut log = EventLog::open(&db_path).expect("open should succeed");

    let chapter_id = ChapterId("2026-06-13".to_string());

    log.append_chapter_open("2026-06-13", "2026-06-13", "daily-v1", "open")
        .expect("append_chapter_open should succeed");

    // Sleep 1 ms between appends to guarantee ULID lexicographic order.
    thread::sleep(Duration::from_millis(1));
    log.append_section(&chapter_id, "morning", "Good morning", "appending", None)
        .expect("first section should succeed");

    thread::sleep(Duration::from_millis(1));
    log.append_section(
        &chapter_id,
        "afternoon",
        "Good afternoon",
        "appending",
        None,
    )
    .expect("second section should succeed");

    thread::sleep(Duration::from_millis(1));
    log.append_section(&chapter_id, "evening", "Good evening", "appending", None)
        .expect("third section should succeed");

    thread::sleep(Duration::from_millis(1));
    log.append_close(&chapter_id, "closed")
        .expect("append_close should succeed");

    // Replay and assert.
    let replay = log
        .chapter(&chapter_id)
        .expect("chapter replay should succeed");

    // Five events total: open + 3 section_append + close.
    assert_eq!(replay.events.len(), 5, "expected 5 events");

    // Verify event types in order.
    let types: Vec<&str> = replay
        .events
        .iter()
        .map(|e| e.event_type.as_str())
        .collect();
    assert_eq!(
        types,
        [
            "open",
            "section_append",
            "section_append",
            "section_append",
            "close"
        ]
    );

    // Event IDs must be strictly ascending (ULID order = time order).
    let ids: Vec<&str> = replay
        .events
        .iter()
        .map(|e| e.event_id.0.as_str())
        .collect();
    for window in ids.windows(2) {
        assert!(
            window[0] < window[1],
            "event IDs must be in ascending order: {} >= {}",
            window[0],
            window[1]
        );
    }

    // Chapter meta reflects closed state.
    assert_eq!(replay.meta.current_state, "closed");
    assert!(
        replay.meta.closed_at.is_some(),
        "closed_at should be set after append_close"
    );

    // Section count check.
    let count = log
        .section_count(&chapter_id, "morning")
        .expect("section_count should succeed");
    assert_eq!(count, 1);
}

/// T2 — Immutability: database-level triggers abort UPDATE and DELETE on `event_log`.
///
/// Also verifies the trigger asymmetry (crux must_not_simplify #2): a raw connection
/// can UPDATE `chapter_meta` on the same database where `event_log` is protected.
#[test]
fn test_immutability_trigger_aborts_update_and_delete() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join(".journal.db");

    let mut log = EventLog::open(&db_path).expect("open should succeed");

    log.append_chapter_open("ch-immutable", "ch-immutable", "schema-v1", "open")
        .expect("append_chapter_open should succeed");

    // Open a separate raw connection to the same database.
    // This bypasses the Rust EventLog API — the trigger must block at DB level.
    let raw = rusqlite::Connection::open(&db_path).expect("raw connection should open");

    // Attempt UPDATE on event_log → must fail with trigger ABORT.
    let update_result = raw.execute("UPDATE event_log SET payload = 'x'", []);
    assert!(
        update_result.is_err(),
        "UPDATE on event_log must be rejected by trigger"
    );
    let update_err = update_result.unwrap_err().to_string();
    assert!(
        update_err.contains("event_log is append-only"),
        "error message must contain 'event_log is append-only', got: {update_err}"
    );

    // Attempt DELETE on event_log → must also fail with trigger ABORT.
    let delete_result = raw.execute("DELETE FROM event_log", []);
    assert!(
        delete_result.is_err(),
        "DELETE on event_log must be rejected by trigger"
    );
    let delete_err = delete_result.unwrap_err().to_string();
    assert!(
        delete_err.contains("event_log is append-only"),
        "error message must contain 'event_log is append-only', got: {delete_err}"
    );

    // Crux must_not_simplify #2: chapter_meta has NO trigger, so UPDATE must succeed
    // on the same raw connection that cannot modify event_log.
    let meta_update_result = raw.execute(
        "UPDATE chapter_meta SET current_state = 'appending' WHERE chapter_id = 'ch-immutable'",
        [],
    );
    assert!(
        meta_update_result.is_ok(),
        "UPDATE on chapter_meta must succeed (no trigger); error: {:?}",
        meta_update_result.unwrap_err()
    );
}

/// T3 — Chapter meta state transition: verifies open → appending → closed progression.
#[test]
fn test_chapter_meta_state_transition() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join(".journal.db");

    let mut log = EventLog::open(&db_path).expect("open should succeed");
    let chapter_id = ChapterId("2026-06-13-transition".to_string());

    // After open: state is "open", closed_at is None.
    log.append_chapter_open(
        "2026-06-13-transition",
        "2026-06-13-transition",
        "schema-v1",
        "open",
    )
    .expect("append_chapter_open should succeed");

    let replay = log.chapter(&chapter_id).expect("chapter should exist");
    assert_eq!(replay.meta.current_state, "open");
    assert!(
        replay.meta.closed_at.is_none(),
        "closed_at should be None after open"
    );

    // After first section: state transitions to "appending".
    thread::sleep(Duration::from_millis(1));
    log.append_section(&chapter_id, "intro", "Hello", "appending", None)
        .expect("append_section should succeed");

    let replay = log.chapter(&chapter_id).expect("chapter should exist");
    assert_eq!(replay.meta.current_state, "appending");
    assert!(
        replay.meta.closed_at.is_none(),
        "closed_at should still be None"
    );

    // After close: state transitions to "closed", closed_at is Some.
    thread::sleep(Duration::from_millis(1));
    log.append_close(&chapter_id, "closed")
        .expect("append_close should succeed");

    let replay = log.chapter(&chapter_id).expect("chapter should exist");
    assert_eq!(replay.meta.current_state, "closed");
    assert!(
        replay.meta.closed_at.is_some(),
        "closed_at must be set after append_close"
    );
}
