# journal-mcp

> *Project canonical history primitive — chapter state machine + multi-schema EventLog + append-only projections, served as an MCP server.*

Project の **正史** (= 判断 + 検証の流れ、 「再開・中断・路線変更判断のための単一参照点」) を AI / Human 共通で読める形で物理化する MCP server。

## Position

`mini-app-mcp` (table-scoped CRUD) / `outline-mcp` (tree-scoped knowledge) / `persona-journal-mcp` (persona-scoped diary) と直交する **project-scoped time-series narrative layer**。 詳細比較は `docs/design.md §1.5` 参照。

## Core Principles

1. **Chapter = compound State Machine entity** — open → appending → closed の transition + section 集合の close 条件 (`sections_present` / `sections_non_empty`) 駆動
2. **Schema-driven section policy** — `append-only-chain` / `append-only-log` / `append-once` / `replace-forbidden` の 4 種を section ごとに宣言
3. **Multi-Schema 並走** — `ytk-canonical-v1` / `madr-v1` / `minimal-v1` 等を同一 project 内に混在可能、 chapter ごとに `chapter_meta.schema_id` で固定
4. **EventLog SoT + 多 Projection** — SQLite EventLog が canonical SoT、 FileProjection (`workspace/journal.md`) / OutlineProjection / MiniAppProjection / GhProjection が読み側 dump
5. **二層 immutability guard** — Rust sealed trait + SQLite `BEFORE UPDATE/DELETE RAISE(ABORT)` trigger

## Status

**WIP — implementation in progress (ST7 complete)**. ST1 (EventLog SQLite primitive), ST2 (ChapterSchema parser + SchemaRegistry + built-in schema embed), ST3 (JournalCore schema-driven state transition engine + ChapterHandle typestate), ST4 (sealed `JournalProjection` trait + `FileProjection` dirty-tracking skeleton + `JournalCore` projection dispatch wiring), ST5 (`FileProjection` full implementation — content-hash dirty-skip + atomic rename + debounce + `SchemaRegistry` accessor helpers), ST6 (`JournalMcpServer` + all 15 `journal_*` MCP tools via `#[tool_router]` + stdio transport + `JournalCore` API extensions), and ST7 (`FileProjection` explicit-only render redesign + hash-check auto-backup guard + `journal_import` 16th tool + EventLog fifth event type `import` + test isolation via `new_with_config`) are implemented and tested.

The MCP server binary (`crates/journal-mcp`) is now functional. Set `JOURNAL_PROJECT_ROOT` to your project directory and run the binary; it serves all 16 tools over stdio transport as specified in `docs/design.md §6` and `§10 Step 5`.

Key behaviour changes introduced in ST7:

- **Explicit-only render** — `close_chapter` no longer auto-renders to `workspace/journal.md`.
  Call `journal_projection_rebuild` explicitly to update the file projection.
- **Hash-check auto-backup** — when `journal_projection_rebuild` writes to an existing
  `workspace/journal.md` whose content differs from the last write (indicating external edits),
  the original is saved as `workspace/journal.md.bak.<epoch_ms>` before overwriting.
- **`journal_import` migration tool** — imports an existing `ytk-canonical-v1` markdown file
  atomically (one SQLite transaction, all chapters land in `closed` state).

See `docs/design.md` for the full design specification. See `CHANGELOG.md` for what has been implemented.

## Layout

```
journal-mcp/
├── Cargo.toml             # workspace (rmcp, schemars, serde_json, tracing-subscriber added ST6)
├── crates/
│   ├── journal/           # core library
│   │   ├── embed/
│   │   │   ├── ytk_canonical_v1.yaml  # ST2: built-in canonical journal schema (compile-time embed)
│   │   │   ├── madr_v1.yaml           # ST2: built-in ADR schema (compile-time embed)
│   │   │   └── minimal_v1.yaml        # ST2: built-in minimal schema (compile-time embed)
│   │   ├── src/
│   │   │   ├── lib.rs                 # public re-exports (updated ST4/ST6/ST7)
│   │   │   ├── event_log.rs           # ST1: EventLog SQLite primitive (append-only SoT);
│   │   │   │                          #   ST6: all_chapter_metas() added;
│   │   │   │                          #   ST7: append_import(payload, tx) + import event replay
│   │   │   ├── schema.rs              # ST2: ChapterSchema YAML parser + SchemaError;
│   │   │   │                          #   ST3: runtime helpers (transition/section/run_hooks) + HookSpec/HookAction/HookWarning;
│   │   │   │                          #   ST5: section_names() / initial_state() accessor helpers
│   │   │   ├── registry.rs            # ST2: SchemaRegistry two-layer resolver (L1 built-in / L2 project-local);
│   │   │   │                          #   ST6: load_from_yaml_str() runtime mutation
│   │   │   ├── core.rs                # ST3: JournalCore schema-driven state transition engine;
│   │   │   │                          #   ST4: projection dispatch wiring;
│   │   │   │                          #   ST6: append_progress / tail_chapters / chapter_ids /
│   │   │   │                          #        open_chapter_ids / progress_of / grep_chapters /
│   │   │   │                          #        list_projection_names / rebuild_projection /
│   │   │   │                          #        load_schema_yaml added;
│   │   │   │                          #   ST7: import_chapter(path) added;
│   │   │   │                          #        close_chapter rebuild dispatch removed (explicit-only)
│   │   │   ├── error.rs               # ST7: JournalError::ImportCollision { chapter_id, existing_epoch_ms } added
│   │   │   ├── handle.rs              # ST3: ChapterHandle<S: ChapterState> compile-time typestate guard (BP-6.2)
│   │   │   ├── projection.rs          # ST4: sealed JournalProjection trait + ProjectionError;
│   │   │   │                          #   ST6: name() required method added to trait
│   │   │   └── projection/
│   │   │       └── file.rs            # ST4: FileProjection skeleton;
│   │   │                              #   ST5: full implementation (content-hash dirty-skip +
│   │   │                              #        atomic rename + debounce);
│   │   │                              #   ST6: name() -> "file" impl;
│   │   │                              #   ST7: last_written_hash field + hash-check auto-backup guard
│   │   └── tests/
│   │       ├── event_log_test.rs       # ST1: 3 integration tests
│   │       ├── schema_registry_test.rs # ST2: 3 integration tests
│   │       ├── journal_core_test.rs    # ST3: 4 integration tests (T1 happy path / T2 AppendOnce / T3 close requires / T4 hook keyword_detect)
│   │       └── projection_test.rs      # ST4: 2 integration tests (dispatch wiring / FileProjection direct);
│   │                                   #   ST5: content-hash dirty-skip + atomic rename tests;
│   │                                   #   ST6: journal_tool_router_count (15 tools assert);
│   │                                   #   ST7: explicit-only render + hash-check backup tests
│   └── journal-mcp/       # stdio MCP server binary
│       ├── Cargo.toml     # ST6: rmcp, schemars, serde_json, anyhow, tokio, tracing-subscriber
│       └── src/
│           └── main.rs    # ST6: JournalMcpServer + #[tool_router] 15 tools + stdio main;
│                          # ST7: journal_import 16th tool + new_with_config test-only API +
│                          #      JOURNAL_DISABLE_FILE_PROJECTION env-var path removed
├── docs/
│   └── design.md          # design specification (this is the SoT);
│                          # ST7: §8.1 import event type added; §13 migration tool 昇格 + Dogfood Reset SOP
└── LICENSE-MIT / LICENSE-APACHE
```

## MCP Tools

All 16 tools are registered in a single `#[tool_router] impl JournalMcpServer` block and served over stdio transport.

| Tool | Category | Description |
|---|---|---|
| `journal_open_chapter` | lifecycle | Open a new chapter with a given name and schema ID |
| `journal_append_section` | lifecycle | Append a section body to an open chapter |
| `journal_append_progress` | lifecycle | Append a progress line to the `Progress` section |
| `journal_close_chapter` | lifecycle | Close a chapter (validates close requirements) |
| `journal_tail` | read | Return the last N chapters |
| `journal_grep` | read | Substring search across all section bodies |
| `journal_chapter_list` | read | List all chapters in Decision Log table format |
| `journal_open_chapters` | read | List IDs of all currently open chapters |
| `journal_progress_of` | read | Return Progress-section events for a chapter |
| `journal_schema_load` | schema | Load a YAML schema into the runtime L2 registry |
| `journal_schema_list` | schema | List all registered schema IDs |
| `journal_schema_show` | schema | Return the full YAML for a registered schema |
| `journal_projection_attach` | projection | Attach a named projection to the server |
| `journal_projection_detach` | projection | Detach a named projection |
| `journal_projection_rebuild` | projection | Replay all chapters through a named projection (explicit-only render trigger) |
| `journal_import` | migration | Import a `ytk-canonical-v1` markdown file atomically (all chapters land in `closed` state; chapter-ID collisions abort the entire import) |

## License

MIT OR Apache-2.0
