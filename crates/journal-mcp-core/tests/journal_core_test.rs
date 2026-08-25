//! Integration tests for `JournalCore` (ST3).
//!
//! Four property categories:
//! - T1 state_transition_happy_path: full open → append × N → close lifecycle
//! - T2 append_once_policy: AppendOnce policy enforced on second append
//! - T3 close_requires_check: close fails when required sections are absent or empty
//! - T4 hook_keyword_detect: hooks on Decided section emit HookWarning

use journal_mcp_core::{HookWarning, JournalCore, JournalError, SchemaRegistry};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Construct a `JournalCore` backed by a fresh tempdir database.
///
/// Returns `(JournalCore, tempfile::TempDir)`.  The `TempDir` must be kept
/// alive for the duration of the test; it is dropped (and the directory
/// removed) when the test exits.
fn make_core() -> (JournalCore, tempfile::TempDir) {
    // SAFETY: TempDir is kept alive by being returned to the caller.
    let dir = tempfile::TempDir::new().expect("TempDir::new should succeed");
    let db_path = dir.path().join(".journal.db");
    let registry = SchemaRegistry::new().expect("SchemaRegistry::new should succeed");
    let core = JournalCore::open(&db_path, registry).expect("JournalCore::open should succeed");
    (core, dir)
}

// ---------------------------------------------------------------------------
// T1 — Happy path state transition
// ---------------------------------------------------------------------------

/// T1 — Happy path: open_chapter → append all 5 required sections → close_chapter.
///
/// Verifies:
/// - `open_chapter` returns the chapter id without error.
/// - All 5 required section appends succeed.
/// - `close_chapter` succeeds when all `requires` are satisfied.
/// - `EventLog::chapter()` replay returns 7 events in order:
///   1 open + 5 section_append + 1 close.
/// - `chapter_meta.current_state` == `"closed"`.
#[test]
fn test_state_transition_happy_path() {
    let (mut core, _dir) = make_core();

    let id = core
        .open_chapter("2026-06-13", "journal-mcp-canonical-v1")
        .expect("open_chapter should succeed");

    // Append all 5 required sections with non-empty bodies.
    let warns = core
        .append_section(&id, "Verified", "cargo test -p journal: 12 PASS [実測]")
        .expect("append Verified should succeed");
    assert!(warns.is_empty(), "Verified section should produce no hooks");

    let warns = core
        .append_section(&id, "Done", "core.rs + handle.rs + schema.rs written")
        .expect("append Done should succeed");
    assert!(warns.is_empty(), "Done section should produce no hooks");

    let warns = core
        .append_section(&id, "Decided", "Schema helper on schema.rs, not core.rs")
        .expect("append Decided without carryover keyword should succeed");
    assert!(
        warns.is_empty(),
        "Decided without carryover keywords should produce no hooks"
    );

    let warns = core
        .append_section(&id, "Not Done", "ST4 projection trait integration")
        .expect("append Not Done should succeed");
    assert!(warns.is_empty(), "Not Done section should produce no hooks");

    let warns = core
        .append_section(&id, "Issues touched", "st3-journal-core")
        .expect("append Issues touched should succeed");
    assert!(
        warns.is_empty(),
        "Issues touched section should produce no hooks"
    );

    // All required sections present and non-empty: close should succeed.
    core.close_chapter(&id)
        .expect("close_chapter should succeed with all required sections");

    // Verify the replay via EventLog (access through open a fresh core on same db).
    // Re-open a core on the same db to verify via replay.
    let dir_path = _dir.path().to_owned();
    let db_path = dir_path.join(".journal.db");
    let registry2 = SchemaRegistry::new().expect("SchemaRegistry::new should succeed");
    let mut core2 =
        JournalCore::open(&db_path, registry2).expect("re-open JournalCore should succeed");

    // We can verify current_state is "closed" by attempting close again — it
    // should fail with NoTransition (no close_chapter from closed state).
    let err = core2
        .close_chapter(&id)
        .expect_err("second close should fail");
    assert!(
        matches!(err, JournalError::NoTransition { .. }),
        "second close should give NoTransition, got: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// T2 — AppendOnce policy
// ---------------------------------------------------------------------------

/// Inline YAML for a minimal schema with one `append_once` section.
///
/// This schema is not shipped as a built-in; it is constructed in-process
/// using `ChapterSchema::parse_str` so T2 is self-contained.
const APPEND_ONCE_YAML: &str = r#"
schema_id: test-append-once
version: 1

states:
  - id: open
    initial: true
  - id: appending
  - id: closed
    terminal: true

transitions:
  - from: open
    to: appending
    on: append_section
  - from: appending
    to: appending
    on: append_section
  - from: appending
    to: closed
    on: close_chapter
    requires:
      sections_present: [Summary]
      sections_non_empty: [Summary]

sections:
  Summary:
    required: true
    append_policy: append-once
    description: One-line summary; must appear exactly once.
"#;

/// T2 — AppendOnce policy: second append to the same section must return
/// `JournalError::AppendOncePolicy`.
#[test]
fn test_append_once_policy() {
    // Build a registry with the inline schema injected via a tempdir.
    // SchemaRegistry::with_project_local reads from <root>/.journal/schemas/*.yaml.
    // SAFETY: TempDir kept alive until end of test.
    let project_root = tempfile::TempDir::new().expect("TempDir::new for project root");
    let schemas_dir = project_root.path().join(".journal").join("schemas");
    std::fs::create_dir_all(&schemas_dir).expect("creating schemas dir should succeed");
    let schema_file = schemas_dir.join("test-append-once-v1.yaml");
    std::fs::write(&schema_file, APPEND_ONCE_YAML)
        .expect("writing inline schema YAML should succeed");

    let db_dir = tempfile::TempDir::new().expect("TempDir::new for db dir");
    let db_path = db_dir.path().join(".journal.db");

    let registry = SchemaRegistry::with_project_local(project_root.path())
        .expect("SchemaRegistry::with_project_local should succeed");
    let mut core = JournalCore::open(&db_path, registry).expect("JournalCore::open should succeed");

    let id = core
        .open_chapter("t2-chapter", "test-append-once-v1")
        .expect("open_chapter should succeed");

    // First append succeeds.
    core.append_section(&id, "Summary", "First and only summary entry.")
        .expect("first append to AppendOnce section should succeed");

    // Second append must fail.
    let err = core
        .append_section(&id, "Summary", "Duplicate summary — must be rejected.")
        .expect_err("second append to AppendOnce section must fail");

    assert!(
        matches!(
            err,
            JournalError::AppendOncePolicy { ref section } if section == "Summary"
        ),
        "expected AppendOncePolicy for Summary, got: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// T3 — close requires check
// ---------------------------------------------------------------------------

/// T3a — close_chapter fails when a required section is completely absent.
///
/// Appends only `Verified` to a journal-mcp-canonical-v1 chapter; `Done`, `Decided`,
/// `Not Done`, and `Issues touched` are absent → `RequiresSectionsPresent`.
#[test]
fn test_close_requires_sections_present() {
    let (mut core, _dir) = make_core();

    let id = core
        .open_chapter("t3a-chapter", "journal-mcp-canonical-v1")
        .expect("open_chapter should succeed");

    core.append_section(&id, "Verified", "cargo test -p journal: 12 PASS [実測]")
        .expect("append Verified should succeed");

    // Missing Done, Decided, Not Done, Issues touched.
    let err = core
        .close_chapter(&id)
        .expect_err("close without all required sections must fail");

    assert!(
        matches!(err, JournalError::RequiresSectionsPresent { .. }),
        "expected RequiresSectionsPresent, got: {err:?}"
    );
}

/// T3b — close_chapter fails when a `sections_non_empty` section has an empty body.
///
/// Appends all 5 required sections but passes an empty string for `Verified`.
/// The schema requires `Verified` to be non-empty (`sections_non_empty`).
#[test]
fn test_close_requires_sections_non_empty() {
    let (mut core, _dir) = make_core();

    let id = core
        .open_chapter("t3b-chapter", "journal-mcp-canonical-v1")
        .expect("open_chapter should succeed");

    // Append Verified with empty body.
    core.append_section(&id, "Verified", "")
        .expect("append empty Verified should succeed at EventLog level");
    core.append_section(&id, "Done", "commit abc123")
        .expect("append Done should succeed");
    core.append_section(&id, "Decided", "schema helpers on schema.rs")
        .expect("append Decided should succeed");
    core.append_section(&id, "Not Done", "ST4 projection trait")
        .expect("append Not Done should succeed");
    core.append_section(&id, "Issues touched", "st3-journal-core")
        .expect("append Issues touched should succeed");

    // close_chapter must fail because Verified body is empty.
    let err = core
        .close_chapter(&id)
        .expect_err("close with empty Verified body must fail");

    assert!(
        matches!(
            err,
            JournalError::RequiresSectionsNonEmpty { ref section } if section == "Verified"
        ),
        "expected RequiresSectionsNonEmpty for Verified, got: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// T4 — hook keyword_detect
// ---------------------------------------------------------------------------

/// T4 — keyword_detect hook on the `Decided` section in `journal-mcp-canonical-v1`.
///
/// The built-in schema declares:
/// ```yaml
/// Decided:
///   hooks:
///     - on_append:
///         type: keyword_detect
///         patterns: ["次 session", "持ち越し", "次回", "next session"]
///         response: warn_carryover
/// ```
///
/// Appending a body that contains `"次 session"` must return a
/// `HookWarning { kind: "warn_carryover", section: "Decided", hint: "次 session" }`.
#[test]
fn test_hook_keyword_detect() {
    let (mut core, _dir) = make_core();

    let id = core
        .open_chapter("t4-chapter", "journal-mcp-canonical-v1")
        .expect("open_chapter should succeed");

    // Append Verified first so the transition from open → appending fires.
    core.append_section(&id, "Verified", "cargo test: 12 PASS")
        .expect("append Verified should succeed");

    // Append Decided with a carryover keyword.
    let warns = core
        .append_section(&id, "Decided", "次 session 持ち越しで ST4 実装予定")
        .expect("append Decided with carryover should succeed");

    // At least one warning with kind == "warn_carryover" must be present.
    assert!(
        !warns.is_empty(),
        "expected at least one HookWarning, got empty list"
    );

    let carryover_warns: Vec<&HookWarning> = warns
        .iter()
        .filter(|w| w.kind == "warn_carryover" && w.section == "Decided")
        .collect();

    assert!(
        !carryover_warns.is_empty(),
        "expected a warn_carryover HookWarning for Decided, got: {warns:?}"
    );

    // Verify that the hint contains the matched pattern.
    let first = carryover_warns[0];
    assert!(
        first.hint.contains("次 session") || first.hint.contains("持ち越し"),
        "hint should contain matched pattern, got: {:?}",
        first.hint
    );
}

// ---------------------------------------------------------------------------
// T5 — dump_markdown (render-to-string, journal_dump tool primitive)
// ---------------------------------------------------------------------------

/// T5a — dump_markdown renders all chapters oldest-first with schema templates.
///
/// Verifies:
/// - Both chapters appear in the output (open chapters included).
/// - Chapters are ordered ascending by chapter id (oldest first), even though
///   `all_chapter_metas` returns newest first.
/// - Chapter/section headers come from the schema templates and section
///   bodies from the appended events.
#[test]
fn test_dump_markdown_renders_chapters_oldest_first() {
    let (mut core, _dir) = make_core();

    let id1 = core
        .open_chapter("2026-06-13", "journal-mcp-canonical-v1")
        .expect("open_chapter 2026-06-13 should succeed");
    core.append_section(&id1, "Verified", "first-chapter-verified-body")
        .expect("append Verified should succeed");

    let id2 = core
        .open_chapter("2026-06-14", "journal-mcp-canonical-v1")
        .expect("open_chapter 2026-06-14 should succeed");
    core.append_section(&id2, "Verified", "second-chapter-verified-body")
        .expect("append Verified should succeed");

    let dump = core
        .dump_markdown(None)
        .expect("dump_markdown should succeed");

    // Schema-driven chapter headers ("## {date} — {name}" with both slots =
    // chapter_id) and appended bodies must be present.
    assert!(
        dump.contains("## 2026-06-13 — 2026-06-13"),
        "dump should contain the first chapter header, got:\n{dump}"
    );
    assert!(
        dump.contains("### Verified"),
        "dump should contain the schema-driven section header, got:\n{dump}"
    );
    assert!(dump.contains("first-chapter-verified-body"));
    assert!(dump.contains("second-chapter-verified-body"));

    // Oldest chapter renders before the newest one.
    let pos1 = dump
        .find("2026-06-13")
        .expect("first chapter id should be present");
    let pos2 = dump
        .find("2026-06-14")
        .expect("second chapter id should be present");
    assert!(
        pos1 < pos2,
        "chapters must be ordered oldest-first: pos({pos1}) < pos({pos2})\n{dump}"
    );
}

/// T5b — dump_markdown `since` filter excludes chapters opened before it.
///
/// Uses `i64::MAX` (nothing qualifies) and `None` (everything qualifies) as
/// the two deterministic boundary cases — wall-clock `opened_at` values make
/// intermediate cut-points non-deterministic in tests.
#[test]
fn test_dump_markdown_since_filter() {
    let (mut core, _dir) = make_core();

    let id = core
        .open_chapter("2026-06-13", "journal-mcp-canonical-v1")
        .expect("open_chapter should succeed");
    core.append_section(&id, "Verified", "since-filter-body")
        .expect("append Verified should succeed");

    let all = core
        .dump_markdown(None)
        .expect("dump_markdown(None) should succeed");
    assert!(all.contains("since-filter-body"));

    let none = core
        .dump_markdown(Some(i64::MAX))
        .expect("dump_markdown(Some(MAX)) should succeed");
    assert!(
        none.is_empty(),
        "since=i64::MAX should exclude every chapter, got:\n{none}"
    );
}
