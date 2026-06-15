# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] — 2026-06-15

### Added

- `crates/journal-mcp-core/src/projection/vector_sqlite.rs`:
  `SqliteVectorProjection` — persistent variant of
  [`VectorProjection`](crate::projection::vector) backed by a plain
  SQLite `BLOB` column (v0.2.0 ε-2)
  - **ε-2 scope**: persistent storage replacing ε-1's in-memory
    `BTreeMap` so embeddings survive process restarts.
  - **Plain SQLite (no extension)**: stores embeddings as f32
    little-endian BLOB in a regular table — no `sqlite-vec` dependency
    (upstream is alpha-stage 0.1.10-alpha.4 with macOS arm64 build
    failures). Linear-scan cosine search is acceptable at the 100-1000
    chapter scale typical of project canonical histories.
  - When `sqlite-vec` stabilizes, a separate `SqliteVecVectorProjection`
    backend can land alongside this one without breaking the API.
  - Storage schema:
    `CREATE TABLE journal_vec_embeddings (chapter_id TEXT PRIMARY KEY,
    embedding BLOB NOT NULL)` — chapter_id keyed, BLOB is f32 LE.
  - `SqliteVectorProjection::open(db_path, client, config)` — idempotent
    constructor; creates the table with `CREATE TABLE IF NOT EXISTS`.
  - `mark_dirty(id)` → `DELETE FROM journal_vec_embeddings WHERE
    chapter_id = ?`.
  - `rebuild_chapter(replay)` → render section bodies → embed → check
    dimension → `INSERT OR REPLACE`.
  - `search(query, limit)` → embed query → SELECT all → linear-scan
    `cosine_similarity` → sort descending → truncate.
  - `fetch_embedding(chapter_id)` / `count()` test-facing inspection
    helpers.
  - 10 new unit tests: rebuild round-trip via BLOB, embeddings persist
    across close + reopen, rebuild replaces existing row, mark_dirty
    deletes row, search ranks exact match first, search respects limit,
    rebuild dimension-mismatch error, search dimension-mismatch error,
    stable `name() == "vector-sqlite"`, blob codec bit-exact round
    trip.
  - `cosine_similarity` (in `vector` module) raised to
    `pub(super)` so the SQLite backend can reuse the same scorer as
    ε-1 — guarantees identical ranking semantics across backends.
  - No new dependencies (uses existing rusqlite). Closes #(internal tracker)
    partial (ε-2 surface; ε-3/ε-4 carry on the same issue).
- `crates/journal-mcp-core/src/projection/vector.rs`: `VectorProjection`
  + `VectorClient` trait — embedding-based semantic search index
  (v0.2.0 ε-1)
  - **ε-1 scope**: projection logic + `VectorClient` trait abstraction
    over the embedding-compute path + `VectorConfig` (dimension) +
    in-memory `BTreeMap<chapter_id, Vec<f32>>` store + cosine-similarity
    `search(query, limit)` method + 9 unit tests using a `MockEmbedder`
    / `FixedEmbedder` / `WrongDimensionEmbedder` recorder
  - **Follow-up commits on the same topic branch land**:
    - ε-2: persistent `sqlite-vec` virtual table backend replacing the
      in-memory store
    - ε-3: concrete `CandleEmbedder` (`VectorClient` impl using
      `candle-core` + `tokenizers` + `hf-hub` to load
      `all-MiniLM-L6-v2` locally, Metal acceleration on Apple Silicon)
    - ε-4: 17th MCP tool `journal_semantic_search(query, project_root?,
      limit?)`
  - `VectorClient` trait — single `embed(text) -> Vec<f32>` method.
    Implementations route to a locally-loaded model (ε-3 candle) or a
    remote HTTP endpoint (Ollama / vLLM / SGLang). Deterministic
    contract (same input → same vector within a process).
  - `VectorConfig` — embedding `dimension` field (default 384 for
    `all-MiniLM-L6-v2`).
  - `VectorProjection<C: VectorClient>` — generic over the embedder so
    ε-3 candle + ε-2 sqlite-vec persistence swap in without changing
    the public method surface.
  - `rebuild_chapter` pipeline: render section bodies (skip non-section
    events) → embed → dimension check → insert or replace in the
    in-memory map. `mark_dirty` is a no-op (full rebuild covers it).
  - `search(query, limit)`: embed query → cosine-similarity against
    every stored embedding → sort descending → truncate to `limit`.
    Returns `Vec<(chapter_id, score)>` where score ∈ `[-1.0, 1.0]`.
    `BTreeMap` iteration gives deterministic tie-breaking.
  - 9 new unit tests: rebuild stores embedding, replaces existing
    embedding, dimension-mismatch error, render_text skips non-section
    events, search ranks exact match first, search respects limit,
    search dimension-mismatch error, stable `name() == "vector"`,
    cosine_similarity hand-verified values.
  - No new dependencies (in-memory store + pure-Rust cosine).
  - Closes #(internal tracker) partial (ε-1 surface; ε-2/ε-3/ε-4 carry on the
    same issue's follow-up commits).
- `crates/journal-mcp-core/src/projection/miniapp_client.rs`:
  `MiniAppCoreClient` — concrete [`MiniAppClient`] impl using
  `mini-app-core` directly (no IPC, no MCP wire) (v0.3.0 δ-2)
  - **Feature flag**: gated behind the optional `miniapp-core` Cargo
    feature so callers that do not need the MiniApp projection do not
    pay the dependency cost.
  - **SDK-direct path**: routes the 4 `MiniAppClient` trait methods to
    the corresponding `mini-app_core::store::Store` async APIs via
    in-process function calls. No `rmcp` child-process spawn, no
    JSON-RPC over stdio. Latency ~ns vs ~ms for the alternative rmcp
    stdio path.
  - **Adapter mechanism rationale**: `mini-app-core` is published on
    crates.io as a "transport-agnostic CRUD library" so SDK-direct is
    the project's intended consumption mode. (The sibling γ-2
    OutlineProjection rmcp client uses a different mechanism because
    outline-mcp's core crate is not published.)
  - **Sync ↔ async bridge**: `block_on(future)` wraps
    `tokio::task::block_in_place` + `Handle::current().block_on` so the
    sync trait methods can drive the async `Store` APIs from inside an
    existing `#[tokio::main]` multi-threaded runtime.
  - **`schema_ensure`**: no-op (mini-app-core's `Store::open` already
    created the table with the supplied schema at `MiniAppCoreClient::open`
    construction time; mini-app-core does not have a separate
    "ensure table exists" API).
  - **Optional dependencies** (gated by the feature flag):
    `mini-app-core 0.11`, `serde_yaml_bw 2.5` (mini-app-core's schema
    parser), `tokio` with `rt-multi-thread` + `macros` features.
  - 4 new integration tests (gated by feature): round-trip
    create/query/update through real SQLite; query returns None when
    absent; schema_ensure is a no-op; open returns Err on malformed
    YAML.
  - Closes #2b562589 (δ-2 wire-up; δ-1 trait + generic + mock landed in
    b855a5e).

### Changed

- `rusqlite` workspace dependency bumped from `0.31` (→ `libsqlite3-sys 0.28`)
  to `0.32` (→ `libsqlite3-sys 0.30`). Required for the upcoming
  `MiniAppCoreClient` (v0.3.0 δ-2, SDK-direct path) which pulls in
  `mini-app-core 0.11` — itself depending on `rusqlite 0.32`. Without the
  bump, `libsqlite3-sys` link conflict prevents the `miniapp-core` feature
  from building. No source changes were required in `journal-mcp-core` /
  `journal-mcp`; all 64 existing unit + integration + doc tests pass
  unchanged on `rusqlite 0.32`.

### Added

- `crates/journal-mcp-core/src/projection/miniapp.rs`: `MiniAppProjection`
  + `MiniAppClient` trait — sync chapter metadata to a mini-app table
  (v0.3.0 δ-1)
  - **δ-1 scope**: projection logic + `MiniAppClient` trait abstraction +
    `MiniAppConfig` (table_name / project_label) + embedded
    `miniapp_schema.yaml` for auto-deploy + 8 unit tests using a
    `MockMiniAppClient` recorder
  - **δ-2 deferred to follow-up commit on the same topic branch
    (sibling to γ-2)**: the concrete `RmcpStdioMiniAppClient` that spawns
    the real `mini-app-mcp` binary and routes calls over stdio via the
    `rmcp` client primitives. Both δ-2 and γ-2 (Outline) share the same
    rmcp child-process wrapper pattern.
  - Row mapping: one row per chapter (keyed by `chapter_id`) with fields
    `chapter_id / project_label / schema_id / current_state / opened_at /
    closed_at / decided_summary / issue_refs[]`. `decided_summary` is the
    first non-empty line of the `Decided` section; `issue_refs` is the
    list of canonical UUIDs (8-4-4-4-12 hex pattern) extracted from the
    `Issues touched` section body. UUID extraction uses a manual
    sliding-window scanner so the crate does not pick up a `regex`
    dependency.
  - `rebuild_chapter` pipeline: lazy `schema_ensure` on first call
    (idempotent for subsequent calls) → build payload (chapter metadata +
    decided_summary + issue_refs) → query existing row by `chapter_id` →
    `row_update` (if exists) or `row_create` (if absent) → clear dirty
    entry on success.
  - 8 new unit tests covering: schema_ensure-called-once, fresh-routes-to-create,
    existing-routes-to-update, decided_summary extraction, issue_refs
    UUID extraction, mark_dirty/rebuild dirty-set lifecycle,
    multi-rebuild idempotent routing, custom config forwarded,
    scan_uuids no false positives on commit hashes / wrong-width hex.
  - Closes #2b562589 (δ-1 surface; δ-2 wire-up tracked separately on
    the same issue's follow-up commit).
- `crates/journal-mcp-core/src/projection/outline.rs`: `OutlineProjection`
  + `OutlineClient` trait — sync chapters as nodes in an Outline-MCP book
  (v0.3.0 γ-1)
  - **γ-1 scope**: projection logic + `OutlineClient` trait abstraction +
    `OutlineConfig` (book_slug / parent_node_path) + 7 unit tests
    using a `MockOutlineClient` recorder
  - **γ-2 deferred to follow-up commit on the same topic branch**: the
    concrete `RmcpStdioOutlineClient` that spawns the real `outline-mcp`
    binary and routes calls over stdio via the `rmcp` client primitives.
    The trait-based split keeps γ-1 self-contained and unit-testable
    without requiring a running `outline-mcp` process.
  - Node mapping: `Outline book = config.book_slug` → parent node
    (`config.parent_node_path`, default `"Chapters"`) → child node per
    chapter, slug = `chapter_id`. Body is rendered Markdown (H1 chapter
    heading + H2 section headings + section bodies).
  - `rebuild_chapter` pipeline: render body → `node_query` for existing
    node → `node_update` (if exists) or `node_create` (if absent) → clear
    dirty entry on success. Idempotent across repeated rebuilds.
  - `mark_dirty` queues the chapter ID into an internal `HashSet`;
    callers may batch-flush via repeated `rebuild_chapter` calls.
  - Non-`section_append` events (open / close / append_progress / import)
    are skipped during body rendering — only section bodies feed the
    human-readable node body.
  - 7 new unit tests covering: fresh-chapter-routes-to-create,
    existing-chapter-routes-to-update, mark_dirty/rebuild dirty-set
    lifecycle, multi-rebuild idempotent routing, render_body skips
    non-section events, custom config forwarded to client, stable
    `name() == "outline"`.
  - Closes #ea35e266 (γ-1 surface; γ-2 wire-up tracked separately on
    the same issue's follow-up commit).
- `crates/journal-mcp-core/src/projection/json.rs`: `JsonProjection` —
  machine-readable JSON dump of all chapters + events (v0.3.0 β)
  - Output: `workspace/journal.json` (or caller-supplied path) with a
    stable envelope: `{schema_version: 1, chapters: [{chapter_id, schema_id,
    current_state, opened_at, closed_at, events: [...]}]}` for jq /
    downstream-agent / CI consumption
  - Chapters emitted in lexicographic chapter_id order (matches
    FileProjection's date-slug → chronological ordering)
  - Atomic write via tempfile + rename: readers observe either complete
    previous content or complete new content, never partial
  - `mark_dirty` is a no-op (full snapshot per rebuild covers the dirty
    chapter implicitly)
  - `rebuild_chapter(replay)` updates the in-memory `BTreeMap` then re-writes
    the entire envelope to disk
  - Pretty-printed (2-space indent) for human readability; size overhead vs
    compact JSON is small relative to event payload size
  - 7 new unit tests (new-does-not-touch-fs, valid-envelope-round-trip,
    idempotency, multi-chapter-lex-ordering, replace-existing-chapter,
    auto-create-parent-dir, mark_dirty-no-op)
  - No new dependencies (`serde_json` already in tree). Closes #75991975.
- `crates/journal-mcp-core/src/projection/fts5.rs`: `FTS5Projection` — SQLite
  FTS5 full-text search index over chapter section bodies (v0.3.0 α)
  - SQLite virtual table `journal_fts` co-located in `.journal.db`, indexed
    by the `trigram` tokenizer so the FTS5 `MATCH` operator behaves like
    SQL `LIKE '%pattern%'` substring search (drop-in semantic compat with
    the existing `LIKE`-based `journal_grep` linear scan); ≥100x speedup
    expected at 1000+ chapters
  - `FTS5Projection::open(db_path)` — idempotent constructor
  - `FTS5Projection::search(pattern)` — substring search helper; pattern
    length must be ≥3 characters (trigram tokenizer requirement)
  - Implements the sealed `JournalProjection` trait
    (`name() == "fts5"` / `mark_dirty` / `rebuild_chapter`); only
    `section_append` events are indexed
  - Multi-connection access against the EventLog DB is WAL-safe
  - 7 new unit tests (open idempotency, rebuild-then-search, rebuild
    idempotency, mark_dirty removal, non-section-event skip, multi-chapter
    isolation, Japanese substring match via trigram)
  - `ProjectionError::Sql(rusqlite::Error)` and
    `ProjectionError::Json(serde_json::Error)` variants added
  - Handler-side wire-up (routing `journal_grep` through the FTS5 fast
    path when attached) lands in a follow-up commit alongside the
    default-attached set decision (master issue bc3b7c79 / design doc §3).
    Closes #7429275b (projection implementation surface; handler wire-up
    tracked separately).
- `crates/journal-mcp/src/main.rs`: per-call `project_root` override on all 16
  MCP tools (multi-project workflow support)
  - Every `Journal*Params` struct gains a `#[serde(default)] pub project_root:
    Option<String>` field — `None` (or omitted) preserves the existing
    startup-time `JOURNAL_PROJECT_ROOT` (or `current_dir()`) behaviour;
    `Some(path)` routes the tool call to a per-project `JournalCore` rooted at
    the given path
  - `JournalMcpServer` gains a lazy cache `extra_cores:
    Arc<Mutex<HashMap<PathBuf, Arc<Mutex<JournalCore>>>>>` keyed by
    canonicalized project_root; the startup-time core is reused for `None` and
    for paths that canonicalize to the startup-time root (short-circuit)
  - `JournalMcpServer::build_core(project_root, attach_file_projection)` —
    static helper that builds a fresh `JournalCore` rooted at the given path;
    shared by the startup-time constructor and the per-call lazy-cache
    populator (`resolve_core`)
  - `JournalMcpServer::resolve_core(project_root: Option<&str>)` — returns the
    `Arc<Mutex<JournalCore>>` handle for the tool call (default core for
    `None`, cache lookup or lazy-build for `Some(path)`)
  - 4 new unit tests covering `resolve_core` semantics: default-passthrough,
    override-creates-separate-db, override-caches-handle, canonical-matches-default
  - Backward-compatible: existing MCP clients that omit `project_root` from
    tool calls see no behaviour change. Closes #b4a5b61b.
- `journal_chapter_list`: pagination via optional `limit` / `offset`
  parameters on `JournalChapterListParams`
  - `limit: Option<usize>` — maximum number of chapters to return,
    applied after `offset`.  `None` (default) returns all remaining
    chapters (= pre-pagination behaviour).
  - `offset: Option<usize>` — number of chapters to skip from the start.
    `None` (default) skips none.  `offset >= total` yields an empty list,
    not an error.
  - Newest chapters first (i.e. `offset=0` is the most recently opened
    chapter).
  - Internal helper `JournalMcpServer::paginate(items, offset, limit)`
    extracted so the slicing semantics are unit-testable without spinning
    up a full `JournalCore` + `tokio` runtime.
  - 6 new unit tests covering `paginate` semantics: omitted-returns-all,
    limit-only, offset-only, limit-and-offset, offset-overflow-yields-empty,
    offset-zero-same-as-none.
  - Backward-compatible: existing MCP clients that omit both fields see no
    behaviour change. Closes #98123835.
- `JournalMcpServer.project_root` is now canonicalized at construction time
  (was: raw `PathBuf` as supplied). This makes the `resolve_core`
  short-circuit reliable on platforms where the supplied path differs from
  its canonical form (e.g. macOS `/var` → `/private/var` for `TempDir`).

### Changed (BREAKING)

- Default schema registry key renamed from `ytk-canonical-v1` to `journal-mcp-canonical-v1`
  (and YAML `schema_id` field from `ytk-canonical` to `journal-mcp-canonical`) to align with
  the project name instead of a personal handle. The embedded YAML file was renamed to
  `crates/journal-mcp-core/embed/journal_mcp_canonical_v1.yaml`. Callers that pass the
  schema ID literal to `open_chapter` / `schema_load` must update the string. Historical
  CHANGELOG entries below (v0.1.0) retain the original `ytk-canonical-v1` literal because
  that was the published API at that time.

### Added (documentation)

- `docs/migration-guide.md`: comprehensive Migration Guide for file-based
  `journal.md` → EventLog (refs: commit `194df30`). Covers schema compliance
  verify, uniform-stub normalization, backup, import execution, rollback,
  and the new append protocol (anti-patterns + fail-loud recipe wrapper).

### Deferred (carry to v0.3.0+)

v0.2.0 release scope は journal-mcp 内の **storage primitive 層**
(FTS5 + Json + γ-1 Outline trait + δ-1/δ-2 MiniApp + ε-1/ε-2 Vector
storage) の完成を確定。 embedding 計算 (concrete embedder model)、 外部
MCP 連携の concrete client、 handler-side wire-up はいずれも external
layer 領分として v0.3.0+ で別途。

- **γ-2**: `OutlineProjection` concrete `rmcp` client (γ-1 trait + mock
  は v0.2.0 着地、 concrete impl が carry)。 outline-mcp 上流が
  single-crate (SDK split crate 未公開) のため、 (a) inline rmcp client
  を journal-mcp-core 側で wrap、 もしくは (c) 別 layer (外部 bridge crate
  / 別 MCP server) で扱う 2 path を v0.3.0+ で再判定。 (issue: 別途
  mini-app 起票)
- **ε-3 CandleEmbedder**: `candle-core` + `tokenizers` + `hf-hub` +
  `all-MiniLM-L6-v2` Metal accel の concrete `VectorClient` impl。
  embedding 計算は journal-mcp 領分外、 external layer (別 crate / 別
  MCP server / 利用側で任意 embedder を inject する path) で扱う方向。
  v0.2.0 では ε-1 trait + ε-2 SQLite storage まで in-tree、 concrete
  embedder は v0.3.0+ で再検討。 (issue (internal tracker))
- **ε-4 17th MCP tool `journal_semantic_search`**: ε-3 sibling、 同上
  carry。 storage primitive は v0.2.0 で揃ったので handler-side で
  user-supplied embedder を受けて search する path は v0.3.0+ で。
  (issue (internal tracker))
- **handler wire-up**: FTS5 / Json / Vector の各 projection を
  default-attached する handler-side 配線、 および `journal_grep` の
  FTS5 fast path への切り替えは v0.3.0+ で別途。

## [0.1.0] — 2026-06-14

### Added

- `crates/journal-mcp-core/src/event_log.rs`: fifth event type `import` added to EventLog schema (ST7)
  - `EventLog::append_import(payload: serde_json::Value, tx: &Transaction)` — writes one
    `import` event row inside an externally-supplied `rusqlite::Transaction`, enabling the caller
    to batch-commit N chapters atomically
  - Replay paths (`replay_chapter` / `replay_until`) extended to expand `import` events: the
    `chapters[]` payload array is unrolled into virtual `chapter_open + section_append × N +
    chapter_close` rows so existing projection / query paths consume imported chapters
    transparently
- `crates/journal-mcp-core/src/core.rs`: `JournalCore::import_chapter` new public method (ST7)
  - `import_chapter(path: &Path) -> Result<Vec<ChapterId>, JournalError>` — reads a
    `ytk-canonical-v1` markdown file, parses H2-delimited chapters and H3-delimited sections,
    opens a single `rusqlite::Transaction`, checks for chapter-ID collisions, writes one
    `import` event with all N chapters in the payload, commits, and returns the imported
    `Vec<ChapterId>`; mid-flight parse errors or collisions trigger full rollback
  - `JournalError::ImportCollision { chapter_id: ChapterId, existing_epoch_ms: i64 }` variant
    added — carries the colliding chapter ID and the timestamp of the pre-existing chapter so
    callers can identify the conflict precisely
- `crates/journal-mcp/src/main.rs`: `journal_import` registered as the 16th MCP tool (ST7)
  - `JournalImportParams { path: String }` — parameter struct with schemars-derived JSON Schema
  - `#[tool] async fn journal_import` handler — delegates to `JournalCore::import_chapter`,
    serialises the returned `Vec<ChapterId>` as a JSON array; annotations:
    `read_only_hint: false, destructive_hint: false, idempotent_hint: false,
    open_world_hint: false` (calling twice with the same file produces a collision error)
  - `test_st7_exactly_sixteen_tools` (renamed from `test_subtask3_exactly_fifteen_tools`) —
    asserts `tool_router.list_all().len() == 16`
  - `EXPECTED_TOOLS` const array extended with `"journal_import"` (element count 16)
- `crates/journal-mcp/src/main.rs`: `JournalMcpServer::new_with_config` test-only constructor
  (ST7-ST3)
  - `pub(crate) fn new_with_config(project_root: PathBuf, attach_file_projection: bool) ->
    anyhow::Result<Self>` — allows tests to construct the server with FileProjection detached;
    gated behind `#[cfg(test)]` so the symbol is absent from production builds; eliminates the
    need for `std::env::set_var("JOURNAL_DISABLE_FILE_PROJECTION", "1")` in test helpers

### Changed

- `crates/journal-mcp-core/src/projection/file.rs`: `FileProjection` redesigned to explicit-only render
  (ST7-ST1)
  - `last_written_hash: Option<u64>` field added to `FileProjection` — tracks the hash of the
    last content written to disk; `None` on first construction
  - `write_atomic` extended with a hash-check overwrite guard: before the atomic rename the
    method reads the existing file (if any), computes its hash, and compares it against
    `last_written_hash`; when they differ (indicating external edits since the last write) the
    existing file is copied to `<path>.bak.<epoch_ms>` before overwriting, protecting
    hand-edited projections
  - `new` and `with_debounce` initialise `last_written_hash: None`
- `crates/journal-mcp-core/src/core.rs`: `close_chapter` no longer dispatches `rebuild_chapter` (ST7-ST1)
  — the `for p in &mut self.projections { p.rebuild_chapter(&final_replay)? }` loop removed;
  `journal_projection_rebuild` (via `JournalCore::rebuild_projection`) is now the sole render
  trigger; `mark_dirty` dispatch from `append_section` is unchanged, so dirty tracking remains
  accurate
- `crates/journal-mcp/src/main.rs`: `JOURNAL_DISABLE_FILE_PROJECTION` env-var path removed
  (ST7-ST3)
  - `JournalMcpServer::new` no longer branches on `JOURNAL_DISABLE_FILE_PROJECTION=1`; the env
    var is no longer read or documented
  - `make_server` test helper replaced with `JournalMcpServer::new_with_config(tmp.path(),
    false)` — eliminates `unsafe { std::env::set_var(...) }` calls that were thread-unsafe
    under Rust 1.81+

### Added (documentation)

- `docs/design.md` §8.1 updated: fifth event type `import` added to EventLog schema table;
  canonical JSON payload form for `import` events documented (ST7-ST4)
- `docs/design.md` §13 updated: migration tool entry changed from "non-goal" to "ST7 昇格 —
  `journal_import` tool 本実装済"; Dogfood Reset SOP section added (ST7-ST4)

---

<!-- ST6 entries below -->

- `crates/journal-mcp/src/main.rs`: `JournalMcpServer` struct + stdio MCP server implementing
  design.md §10 Step 5 — exposes all 15 `journal_*` tools via `#[tool_router]` macro (ST6)
  - `JournalMcpServer` — `Clone`-able server struct wrapping `Arc<Mutex<JournalCore>>`; all 15
    tools registered in a single `#[tool_router] impl JournalMcpServer` block (Crux #1)
  - `JournalMcpServer::new(project_root)` — initialises `SchemaRegistry::with_project_local`,
    opens `{project_root}/workspace/.journal.db`, and optionally attaches `FileProjection` to
    `{project_root}/workspace/journal.md` (skipped when `JOURNAL_DISABLE_FILE_PROJECTION=1`)
  - `#[tool_handler] impl ServerHandler for JournalMcpServer` with `get_info()` returning
    `ServerInfo { name: "journal-mcp", version: env!("CARGO_PKG_VERSION") }`
  - `main()` — resolves `JOURNAL_PROJECT_ROOT` env (falls back to `current_dir()`), constructs
    `JournalMcpServer`, and calls `server.serve(stdio()).await?.waiting().await?` (Crux #3: stdio
    transport fixed; TCP/HTTP alternatives are prohibited)
  - **Chapter lifecycle tools** (writer, `idempotent_hint: false`):
    - `journal_open_chapter(name, schema_id)` → chapter ID string
    - `journal_append_section(chapter_id, section_name, body)` → JSON array of `HookWarning`
    - `journal_append_progress(chapter_id, line)` → JSON array of `HookWarning` (thin wrapper
      over `append_section` targeting the `"Progress"` section)
    - `journal_close_chapter(chapter_id)` → `"ok"`
  - **Schema tools** — 3 independent MCP tool entries (Crux #2):
    - `journal_schema_load(yaml)` — loads a YAML literal into the runtime L2 registry via
      `JournalCore::load_schema_yaml`; returns the registry key (writer, `idempotent_hint: true`)
    - `journal_schema_list()` — returns all registered schema IDs as a JSON array (read-only)
    - `journal_schema_show(key)` — returns the full YAML for a given registry key (read-only)
  - **Read tools** (read-only, `idempotent_hint: true`):
    - `journal_tail(n?)` — returns the last `n` chapters (default 10) as JSON
    - `journal_grep(pattern, since?, until?)` — full-text substring search across all section
      bodies; optional `since`/`until` Unix epoch ms filters
    - `journal_chapter_list()` — returns all chapters in Microsoft Decision Log table format
      (`chapter_id`, `schema_id`, `current_state`, `opened_at`, `closed_at`, `decided_summary`,
      `link`)
    - `journal_open_chapters()` — returns chapter IDs of all currently open chapters
    - `journal_progress_of(chapter_id)` — returns all Progress-section events for a chapter
  - **Projection tools**:
    - `journal_projection_attach(name)` — attaches a named projection; first cut only supports
      `"file"` (writer, `idempotent_hint: true`)
    - `journal_projection_detach(name)` — detaches a named projection (returns
      `JournalError::Unsupported` in first cut; tool entry registered per Crux #1)
    - `journal_projection_rebuild(name)` — replays all chapters through the named projection
      (writer, `idempotent_hint: true`)
  - Parameter structs per tool: each `#[derive(Debug, Deserialize, JsonSchema)]` struct has doc
    comments that become the MCP wire `description` fields via schemars (BP-3 pattern)
- `crates/journal-mcp/Cargo.toml`: added `rmcp = "0.2"`, `schemars = { workspace = true }`,
  `serde_json = { workspace = true }`, `anyhow = { workspace = true }`,
  `tokio = { workspace = true }`, `tracing-subscriber = { workspace = true }` dependencies
- `Cargo.toml` (workspace): added `rmcp`, `schemars`, `serde_json`, `tracing-subscriber` to
  `[workspace.dependencies]`
- `crates/journal-mcp-core/src/core.rs`: 8 new public methods extending `JournalCore` (ST6 Core API)
  - `append_progress(&mut self, id: &ChapterId, line: &str) -> Result<Vec<HookWarning>, JournalError>`
    — thin wrapper delegating to `append_section(id, "Progress", line)`
  - `tail_chapters(&mut self, n: usize) -> Result<Vec<ChapterReplay>, JournalError>` — returns
    the last `n` chapters ordered by `opened_at DESC` via `EventLog::all_chapter_metas`
  - `chapter_ids(&mut self, since: Option<i64>) -> Result<Vec<ChapterId>, JournalError>` — all
    chapter IDs; optional `since` Unix epoch ms filter
  - `open_chapter_ids(&mut self) -> Result<Vec<ChapterId>, JournalError>` — chapter IDs where
    `closed_at IS NULL`
  - `progress_of(&mut self, id: &ChapterId) -> Result<Vec<EventRow>, JournalError>` — filters
    chapter event replay to `section_name == "Progress"` rows
  - `grep_chapters(&mut self, pattern: &str, since: Option<i64>, until: Option<i64>) -> Result<Vec<ChapterReplay>, JournalError>`
    — substring match on all section body fields; optional time range filter
  - `list_projection_names(&self) -> Vec<&'static str>` — returns `name()` of every attached
    projection
  - `rebuild_projection(&mut self, name: &str) -> Result<(), JournalError>` — replays all
    chapters through the named projection via `EventLog::all_chapter_metas` iteration
  - `load_schema_yaml(&mut self, yaml: &str) -> Result<String, JournalError>` — facade over
    `SchemaRegistry::load_from_yaml_str`; avoids exposing `&mut SchemaRegistry` from `JournalCore`
- `crates/journal-mcp-core/src/event_log.rs`: `all_chapter_metas(n: Option<usize>) -> Result<Vec<ChapterMeta>, EventLogError>`
  — SQL `SELECT * FROM chapter_meta ORDER BY opened_at DESC` with optional `LIMIT`; used by
  `tail_chapters`, `chapter_ids`, `open_chapter_ids`, `grep_chapters`, `rebuild_projection`
- `crates/journal-mcp-core/src/registry.rs`: `load_from_yaml_str(&mut self, yaml: &str) -> Result<String, RegistryError>`
  — parses a YAML literal via `ChapterSchema::parse_str`, derives the registry key
  (`{schema_id}-v{version}`), inserts into L2, and returns the key
- `crates/journal-mcp-core/src/projection.rs`: `fn name(&self) -> &'static str` added as a required
  method to the `JournalProjection` trait; enables named lookup in
  `list_projection_names` / `rebuild_projection` / `projection_attach` / `projection_detach`
- `crates/journal-mcp-core/src/projection/file.rs`: `FileProjection` full implementation (ST5)
  - Content-hash dirty-skip: `rebuild_chapter` computes SHA-256 of the rendered output and
    skips the file write if the existing file matches (avoids redundant I/O)
  - Dirty-chapter marking: `mark_dirty` inserts the chapter ID into the `dirty` `HashSet`
  - Atomic rename: writes to a temp file (`journal.md.tmp`) then renames to the target path
    via `std::fs::rename` to prevent torn reads during rebuild
  - Debounce guard: optional minimum interval between rebuilds via an internal `last_rebuilt`
    timestamp (protects against hot-loop append storms)
  - `pub fn name(&self) -> &'static str { "file" }` — satisfies the new `JournalProjection::name`
    required method
  - `crates/journal-mcp-core/src/schema.rs`: `accessor_field` helpers added to `ChapterSchema` — new
    `section_names()` and `initial_state()` accessors consumed by `FileProjection` template render
- `crates/journal-mcp-core/tests/projection_test.rs`: integration tests extended for ST5 and ST6
  - Content-hash dirty-skip test (ST5): writes a chapter, rebuilds, asserts file written; then
    rebuilds again with no changes and asserts the file mtime is unchanged
  - Atomic rename regression test (ST5): verifies the temp file is absent after a successful
    `rebuild_chapter`
  - `journal_tool_router_count` (ST6 T1): constructs `JournalMcpServer` with
    `JOURNAL_DISABLE_FILE_PROJECTION=1` and asserts `tool_router.list_all().len() == 15`

- `crates/journal-mcp-core/src/projection.rs`: sealed `JournalProjection` trait implementing design.md §4
  - `pub(crate) mod private { pub trait Sealed {} }` — sealing mechanism; external crates cannot
    implement `JournalProjection` (the `private` module is not visible outside the `journal-mcp-core` crate)
  - `pub trait JournalProjection: private::Sealed` with two required methods:
    `mark_dirty(&mut self, id: &ChapterId) -> Result<(), ProjectionError>` and
    `rebuild_chapter(&mut self, replay: &ChapterReplay) -> Result<(), ProjectionError>`
  - `pub enum ProjectionError` via `thiserror::Error` derive with `Io(#[from] std::io::Error)` variant
    (unused in ST4; reserved for ST5 file IO paths; suppressed with `#[allow(dead_code)]`)
  - `compile_fail` doctest verifying that `impl JournalProjection for ExternalType {}` is a
    compile error (follows `handle.rs` sealed trait guard pattern)
- `crates/journal-mcp-core/src/projection/file.rs`: `FileProjection` skeleton implementing design.md §8.2
  - `pub struct FileProjection { dirty: std::collections::HashSet<ChapterId> }` — dirty-chapter
    tracking; rebuild content is ST5 scope only (Crux constraint: no file IO or template render
    in ST4)
  - `pub fn new() -> Self` — constructs an empty `FileProjection`
  - `pub fn dirty_chapters(&self) -> &HashSet<ChapterId>` — accessor for test inspection
  - `impl private::Sealed for FileProjection {}` — satisfies sealed trait requirement via
    `pub(crate)` visibility of `projection::private`
  - `impl JournalProjection for FileProjection`: `mark_dirty` inserts the chapter id into the
    dirty set; `rebuild_chapter` removes the chapter id from the dirty set and returns `Ok(())`
    stub (full file reconstruction deferred to ST5)
- `crates/journal-mcp-core/src/core.rs`: `JournalCore` projection dispatch wiring (design.md §7)
  - `projections: Vec<Box<dyn JournalProjection>>` field added to `JournalCore`; initialized as
    `Vec::new()` in `JournalCore::open`
  - `pub fn add_projection<P: JournalProjection + 'static>(&mut self, p: P)` — registers a
    projection; boxed and appended to the `projections` vec
  - `append_section`: dispatch loop calling `p.mark_dirty(id)?` for each projection immediately
    before returning `Ok(warnings)`
  - `close_chapter`: replays the final chapter state via `self.log.chapter(id)?` after the close
    write, then calls `p.rebuild_chapter(&final_replay)?` for each projection immediately before
    returning `Ok(())`
  - `JournalError::Projection(#[from] ProjectionError)` variant added; all dispatch errors
    propagate via `?` with `tracing::warn!` on error
  - `#[cfg(test)] struct TestProjection` with `AtomicUsize` counters for `mark_dirty_calls` and
    `rebuild_calls`, plus `impl private::Sealed for TestProjection {}` — used to verify dispatch
    wiring in integration tests
- `crates/journal-mcp-core/src/lib.rs`: `pub mod projection` declaration and re-exports
  (`FileProjection`, `JournalProjection`, `ProjectionError`)
- `crates/journal-mcp-core/tests/projection_test.rs`: integration tests for ST4
  - `test_file_projection_mark_dirty_and_rebuild` (T2 / property test) — constructs
    `FileProjection` directly, calls `mark_dirty`, asserts chapter id appears in
    `dirty_chapters()`, calls `rebuild_chapter`, asserts the id is removed
  - `test_core_dispatch_wiring` (T3 / dispatch test) — attaches `TestProjection` to `JournalCore`
    via `add_projection`, runs `open_chapter` → `append_section` → `close_chapter`, asserts
    `mark_dirty_calls == 1` and `rebuild_calls == 1`

- `crates/journal-mcp-core`: new crate `journal-mcp-core` — project canonical history primitive
- `crates/journal-mcp-core/src/event_log.rs`: `EventLog` SQLite primitive implementing design.md §8.1
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
- `crates/journal-mcp-core/tests/event_log_test.rs`: 3 integration tests
  - `test_happy_path_chapter_lifecycle` — open → section × N → close, verifies replay ordering
  - `test_immutability_trigger_aborts_update_and_delete` — raw `Connection` UPDATE/DELETE on
    `event_log` returns `Err` with `'event_log is append-only'` substring; same UPDATE on
    `chapter_meta` succeeds
  - `test_chapter_meta_state_transition` — verifies `open → appending → closed` UPDATE path
    through public API
- `crates/journal-mcp-core/Cargo.toml`: added `[dev-dependencies]` section with `tempfile = "3"`
- `crates/journal-mcp-core/src/lib.rs`: `pub mod event_log` declaration and public re-exports
  (`EventLog`, `ChapterId`, `EventId`, `ChapterMeta`, `ChapterReplay`, `EventRow`,
  `EventLogError`)
- `crates/journal-mcp-core/src/schema.rs`: `ChapterSchema` YAML parser implementing design.md §5
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
- `crates/journal-mcp-core/src/registry.rs`: `SchemaRegistry` two-layer schema resolver implementing
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
- `crates/journal-mcp-core/embed/ytk_canonical_v1.yaml`: built-in `ytk-canonical-v1` schema (version 1)
  embedded at compile time — defines the canonical journal chapter state machine with
  `open → appending → closed` transitions and section policies for `Verified`, `Done`,
  `Decided`, `Not Done`, `Issues touched`, `Cleanup pending`, `Rejected paths`, `Progress`,
  `Notes`
- `crates/journal-mcp-core/embed/madr_v1.yaml`: built-in `madr-v1` schema (version 1) embedded at
  compile time — lightweight ADR schema with `proposed → accepted / rejected` transitions
- `crates/journal-mcp-core/embed/minimal_v1.yaml`: built-in `minimal-v1` schema (version 1) embedded
  at compile time — minimal two-state schema for simple log entries
- `crates/journal-mcp-core/tests/schema_registry_test.rs`: integration tests for ST2
  - `test_parse_and_fetch_builtin_schemas` — verifies all three built-in schemas are fetchable
    via `SchemaRegistry::new()` without any project-local directory present
  - `test_unknown_section_reject` — verifies `ChapterSchema::parse_str` returns
    `SchemaError::UnknownSection` for a schema referencing a section name not declared in
    `sections`
  - `test_l2_overrides_l1` — places a modified `ytk-canonical-v1` YAML in a tempdir
    `.journal/schemas/`, calls `with_project_local`, and asserts the L2 entry is returned by
    `get("ytk-canonical-v1")` instead of the L1 built-in
- `crates/journal-mcp-core/src/lib.rs`: added `pub mod schema` / `pub mod registry` declarations and
  explicit re-exports (`ChapterSchema`, `SchemaError`, `SectionSpec`, `AppendPolicy`,
  `StateSpec`, `TransitionSpec`, `SchemaRegistry`, `RegistryError`)
- `crates/journal-mcp-core/src/core.rs`: `JournalCore` — schema-driven state transition engine
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
- `crates/journal-mcp-core/src/handle.rs`: `ChapterHandle<S: ChapterState>` — compile-time typestate
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
- `crates/journal-mcp-core/src/schema.rs`: runtime schema query helpers added (design.md §7 pseudo-code)
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
- `crates/journal-mcp-core/src/lib.rs`: added `pub mod core` / `pub mod handle` declarations and
  re-exports (`JournalCore`, `JournalError`, `HookSpec`, `HookAction`, `HookWarning`)
- `crates/journal-mcp-core/tests/journal_core_test.rs`: 4 integration tests for ST3
  - `test_state_transition_happy_path` (T1) — `open_chapter` → `append_section` × 5 required
    sections → `close_chapter`; verifies replay event count and that a second `close_chapter`
    returns `JournalError::NoTransition`
  - `test_append_once_policy` (T2) — second `append_section` on an `append-once` section
    returns `JournalError::AppendOncePolicy`
  - `test_close_requires_check` (T3) — `close_chapter` without required sections returns
    `JournalError::RequiresSectionsPresent` / `RequiresSectionsNonEmpty`
  - `test_hook_keyword_detect` (T4) — appending body containing a keyword from
    `ytk_canonical_v1.yaml` `Decided` section hooks produces a non-empty `Vec<HookWarning>`
