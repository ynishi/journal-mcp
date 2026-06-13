# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `crates/journal`: new crate `journal` — project canonical history primitive
- `crates/journal/src/event_log.rs`: `EventLog` SQLite primitive implementing design.md §8.1
  - `event_log` table (7 columns, STRICT) with `BEFORE UPDATE` / `BEFORE DELETE` triggers that
    `RAISE(ABORT, 'event_log is append-only')` — database-level append-only guarantee
  - `chapter_meta` table (5 columns, STRICT) without immutability triggers, enabling state
    transitions `open → appending → closed` via UPDATE on the same connection
  - PRAGMA setup: `journal_mode=WAL`, `synchronous=NORMAL`, `foreign_keys=ON` applied on every
    `Connection::open`
  - `ChapterId` / `EventId` public newtypes (ULID-backed strings)
  - `EventLog::open` — idempotent schema init (`CREATE TABLE IF NOT EXISTS` + `CREATE TRIGGER IF
    NOT EXISTS`)
  - `append_chapter_open` / `append_section` / `append_close` — append-only write API
  - `chapter_meta` / `section_count` / `chapter` (replay) — read API
  - `EventLogError` via `thiserror::Error` derive with `Sqlite` / `Time` / `Json` /
    `ChapterNotFound` / `InvalidState` variants
- `crates/journal/tests/event_log_test.rs`: 3 integration tests
  - `test_happy_path_chapter_lifecycle` — open → section × N → close, verifies replay ordering
  - `test_immutability_trigger_aborts_update_and_delete` — raw `Connection` UPDATE/DELETE on
    `event_log` returns `Err` with `'event_log is append-only'` substring; same UPDATE on
    `chapter_meta` succeeds
  - `test_chapter_meta_state_transition` — verifies `open → appending → closed` UPDATE path
    through public API
- `crates/journal/Cargo.toml`: added `[dev-dependencies]` section with `tempfile = "3"`
- `crates/journal/src/lib.rs`: `pub mod event_log` declaration and public re-exports
  (`EventLog`, `ChapterId`, `EventId`, `ChapterMeta`, `ChapterReplay`, `EventRow`,
  `EventLogError`)
