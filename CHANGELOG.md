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
- `crates/journal/src/schema.rs`: `ChapterSchema` YAML parser implementing design.md §5
  - `ChapterSchema::parse_str` — deserializes a schema YAML string into a validated struct;
    rejects schemas with unknown section references in `transitions[].requires.sections_present`
    / `sections_non_empty` via `SchemaError::UnknownSection`
  - `SectionSpec` — per-section policy (`required`, `append_policy`, `evidence_required`,
    `description`)
  - `AppendPolicy` enum — four variants (`AppendOnlyChain` / `AppendOnlyLog` / `AppendOnce` /
    `ReplaceForbidden`), deserialized from kebab-case YAML values
  - `StateSpec` / `TransitionSpec` / `TransitionRequires` — state machine spec structs, all
    `Clone + PartialEq + Eq`
  - `SchemaError` via `thiserror::Error` with `Parse` / `UnknownSection` / `MissingField` /
    `InvalidAppendPolicy` variants
- `crates/journal/src/registry.rs`: `SchemaRegistry` two-layer schema resolver implementing
  design.md §9.3
  - `#[derive(RustEmbed)] #[folder = "embed/"] struct EmbeddedSchemas` — compile-time embed
    of the three built-in YAML files; no runtime filesystem access for built-ins
  - `SchemaRegistry::new()` — loads L1 built-in schemas from embedded bytes only; succeeds
    even when no `.journal/schemas/` directory exists (zero-config guarantee)
  - `SchemaRegistry::with_project_local(project_root)` — loads L1 built-ins then scans
    `<project_root>/.journal/schemas/*.yaml` as L2; directory absence is silently skipped,
    per-file parse failures emit `tracing::warn!` and continue
  - `SchemaRegistry::get(schema_id)` — resolves by checking L2 before L1, returning the L2
    entry whenever both layers contain the same `schema_id` (L2 overrides L1 invariant)
  - `SchemaRegistry::list()` — returns deduplicated `schema_id` list (L2 entries shadow L1
    duplicates)
  - `RegistryError` via `thiserror::Error` with `BuiltinLoad` / `ProjectLocalLoad` / `Io`
    variants
- `crates/journal/embed/ytk_canonical_v1.yaml`: built-in `ytk-canonical-v1` schema (version 1)
  embedded at compile time — defines the canonical journal chapter state machine with
  `open → appending → closed` transitions and section policies for `Verified`, `Done`,
  `Decided`, `Not Done`, `Issues touched`, `Cleanup pending`, `Rejected paths`, `Progress`,
  `Notes`
- `crates/journal/embed/madr_v1.yaml`: built-in `madr-v1` schema (version 1) embedded at
  compile time — lightweight ADR schema with `proposed → accepted / rejected` transitions
- `crates/journal/embed/minimal_v1.yaml`: built-in `minimal-v1` schema (version 1) embedded
  at compile time — minimal two-state schema for simple log entries
- `crates/journal/tests/schema_registry_test.rs`: integration tests for ST2
  - `test_parse_and_fetch_builtin_schemas` — verifies all three built-in schemas are fetchable
    via `SchemaRegistry::new()` without any project-local directory present
  - `test_unknown_section_reject` — verifies `ChapterSchema::parse_str` returns
    `SchemaError::UnknownSection` for a schema referencing a section name not declared in
    `sections`
  - `test_l2_overrides_l1` — places a modified `ytk-canonical-v1` YAML in a tempdir
    `.journal/schemas/`, calls `with_project_local`, and asserts the L2 entry is returned by
    `get("ytk-canonical-v1")` instead of the L1 built-in
- `crates/journal/src/lib.rs`: added `pub mod schema` / `pub mod registry` declarations and
  explicit re-exports (`ChapterSchema`, `SchemaError`, `SectionSpec`, `AppendPolicy`,
  `StateSpec`, `TransitionSpec`, `SchemaRegistry`, `RegistryError`)
- `crates/journal/src/core.rs`: `JournalCore` — schema-driven state transition engine
  implementing design.md §7
  - `JournalCore::open(path, registry)` — opens (or creates) the `.journal.db` at `path`
    and binds a `SchemaRegistry`; delegates all persistent writes to `EventLog`
  - `JournalCore::open_chapter(name, schema_id)` — resolves the schema, derives the initial
    state via `ChapterSchema::initial_state`, and delegates `EventLog::append_chapter_open`;
    returns a `ChapterId`
  - `JournalCore::append_section(id, name, body)` — replays chapter state from `EventLog`,
    validates the `AppendPolicy` (rejects `AppendOnce` sections that already have content),
    runs `schema.run_hooks` to emit `HookWarning` values, and delegates
    `EventLog::append_section`; returns `Vec<HookWarning>`
  - `JournalCore::close_chapter(id)` — validates all `sections_present` and
    `sections_non_empty` close requirements against the event replay, then delegates
    `EventLog::append_close`
  - `JournalError` via `thiserror::Error` with `Schema` / `EventLog` / `Registry` /
    `SchemaNotFound` / `UnknownState` / `NoTransition` / `UnknownSection` /
    `AppendOncePolicy` / `RequiresSectionsPresent` / `RequiresSectionsNonEmpty` variants;
    all `Err` arms emit `tracing::warn!`
  - Crux invariants enforced: (1) every persistent write path calls `self.log.append_*` with
    no other storage path; (2) `JournalCore` holds only `log: EventLog` and
    `registry: SchemaRegistry`, never a raw `rusqlite::Connection`
- `crates/journal/src/handle.rs`: `ChapterHandle<S: ChapterState>` — compile-time typestate
  guard for chapter state transitions (design.md §7.1, BP-6.1 sealed trait + BP-6.2
  PhantomData typestate)
  - `ChapterState` sealed trait (`pub(crate)`) via `mod private { pub trait Sealed {} }`;
    external code cannot implement new state types
  - Zero-sized marker structs `Open`, `Appending`, `Closed` — each implements `ChapterState`
    and `private::Sealed`
  - `ChapterHandle<S>` holds `id: ChapterId`, `schema: Arc<ChapterSchema>`, and
    `PhantomData<S>`; all fields `pub(crate)`
  - `impl ChapterHandle<Open>`: `new`, `start_appending → ChapterHandle<Appending>`
  - `impl ChapterHandle<Appending>`: `close → ChapterHandle<Closed>`
  - `impl ChapterHandle<Closed>`: `id()` only — `append_section` and `close` are
    **intentionally absent**, so any attempt to call them on a `Closed` handle is a
    **compile error** (verified by `compile_fail` doc test in the module)
- `crates/journal/src/schema.rs`: runtime schema query helpers added (design.md §7 pseudo-code)
  - `ChapterSchema::transition(current, event)` — iterates `transitions` to find the entry
    where `from == current && on == event`; returns `SchemaError::UnknownState` when `current`
    does not match any `from`, `SchemaError::NoTransition` when no entry matches `event`
  - `ChapterSchema::section(name)` — looks up `sections.get(name)`;
    returns `SchemaError::SectionNotFound` when absent
  - `ChapterSchema::run_hooks(section_name, body)` — iterates `SectionSpec.hooks` for the
    named section, executes `HookAction::KeywordDetect` by checking each pattern as a
    case-insensitive substring, and collects `HookWarning` values for every match
  - `SectionSpec.hooks: Vec<HookSpec>` — new field with `#[serde(default)]`; existing YAML
    files without a `hooks:` key deserialise to an empty Vec (backwards-compatible)
  - New types: `HookSpec { on_append: HookAction }`,
    `HookAction::KeywordDetect { patterns: Vec<String>, response: String }`,
    `HookWarning { kind: String, hint: String }` (public, re-exported from `lib.rs`)
  - `SchemaError` new variants: `UnknownState` / `NoTransition` / `SectionNotFound`
    (runtime-only; do not affect existing `parse_str` paths)
- `crates/journal/src/lib.rs`: added `pub mod core` / `pub mod handle` declarations and
  re-exports (`JournalCore`, `JournalError`, `HookSpec`, `HookAction`, `HookWarning`)
- `crates/journal/tests/journal_core_test.rs`: 4 integration tests for ST3
  - `test_state_transition_happy_path` (T1) — `open_chapter` → `append_section` × 5 required
    sections → `close_chapter`; verifies replay event count and that a second `close_chapter`
    returns `JournalError::NoTransition`
  - `test_append_once_policy` (T2) — second `append_section` on an `append-once` section
    returns `JournalError::AppendOncePolicy`
  - `test_close_requires_check` (T3) — `close_chapter` without required sections returns
    `JournalError::RequiresSectionsPresent` / `RequiresSectionsNonEmpty`
  - `test_hook_keyword_detect` (T4) — appending body containing a keyword from
    `ytk_canonical_v1.yaml` `Decided` section hooks produces a non-empty `Vec<HookWarning>`
