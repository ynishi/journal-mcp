# Projection Suite Design — `journal-mcp` v0.3.0

> *Five new `JournalProjection` implementations expanding the EventLog
> consumption surface: full-text search, machine-readable JSON dump,
> Outline-MCP knowledge tree sync, mini-app cross-search row sync, and
> embedding-based semantic search.*

This document specifies the v0.3.0 projection suite expansion, branching from
`main` at the v0.2.0 baseline (commits `53d48c6` per-call `project_root`
override + `817c2aa` `journal_chapter_list` pagination).

---

## 1. Overview

### 1.1 Position in the architecture

`journal-mcp-core` separates **EventLog** (canonical SoT in SQLite) from
**Projections** (read-side derived state). v0.1.0–v0.2.0 ships a single
`FileProjection` that renders `workspace/journal.md`. v0.3.0 adds five
projections that consume the same EventLog and write to independent targets:

```
                          ┌────────────────────────────┐
                          │  EventLog (SQLite SoT)     │
                          │  workspace/.journal.db     │
                          └──────────────┬─────────────┘
                                         │ replay events
                  ┌──────────┬───────────┼──────────┬──────────┬──────────┐
                  ▼          ▼           ▼          ▼          ▼          ▼
        ┌──────────────┐ ┌────────┐ ┌────────┐ ┌────────┐ ┌────────────┐ ┌──────────┐
        │ FileProjection│ │ FTS5  │ │ Json   │ │Outline │ │ MiniApp    │ │ Vector   │
        │ journal.md   │ │ index │ │.json   │ │MCP node│ │ MCP row    │ │ embedding│
        │ (existing)   │ │(new)  │ │ (new)  │ │ (new)  │ │ (new)      │ │  (new)   │
        └──────────────┘ └────────┘ └────────┘ └────────┘ └────────────┘ └──────────┘
                                         │
                                    (read-side, write-once-per-chapter)
```

### 1.2 Sealed trait contract (unchanged)

`crates/journal-mcp-core/src/projection.rs` already exposes a sealed
`JournalProjection` trait with three required methods:

- `name() -> &'static str` — stable identifier for `journal_projection_attach`
  / `journal_projection_rebuild`
- `mark_dirty(chapter_id)` — invalidates the projection's cached view of a
  single chapter (called from `append_section` / `append_progress`)
- `rebuild_chapter(replay)` — re-renders the projection's view of a single
  chapter from the EventLog replay (called from `journal_projection_rebuild`)

All five new projections implement this same trait — no trait surface change
is required.

### 1.3 Backward compatibility

- v0.1.0–v0.2.0 callers see no behaviour change. The default attached
  projection set remains `FileProjection` for `JournalMcpServer::new()`.
- New projections are **opt-in** via configuration (env var or attach call).
- Default-attached set may be expanded in v0.3.0 to include FTS5 + Json
  (both are internal-only, no external dependencies) — see §7 Migration.

---

## 2. JournalProjection inventory

### 2.1 `FTS5Projection` — full-text search index

**Purpose**. Replace the `LIKE`-based linear scan in `journal_grep` with a
SQLite FTS5 virtual table. At 1000+ chapters the speedup is ≥100x.

**Storage**. SQLite virtual table in the same `.journal.db`:

```sql
CREATE VIRTUAL TABLE journal_fts USING fts5(
    chapter_id,
    section_name,
    body,
    tokenize = 'unicode61 remove_diacritics 2'
);
```

The `unicode61` tokenizer handles Japanese (and other Unicode languages)
adequately for substring search; advanced CJK tokenization (kuromoji-style)
is out of scope.

**Lifecycle**.

- `mark_dirty(chapter_id)` — `DELETE FROM journal_fts WHERE chapter_id = ?`
- `rebuild_chapter(replay)` — re-insert one row per `section_append` event

**Integration**. `journal_grep` handler routes through the FTS5 index when
the `FTS5Projection` is attached (default-attached candidate); falls back
to the existing `LIKE` scan when absent (backward-compat for
custom-configured servers).

**Implementation footprint**. ~150 LOC + 1 dependency feature
(`rusqlite` `bundled-full` for FTS5 enable).

**MCP tool surface**. No new tools — `journal_grep` gains a fast path.

---

### 2.2 `JsonProjection` — machine-readable JSON dump

**Purpose**. Programmatic consumers (jq pipelines, downstream agents, CI
jobs) can read all chapters + events as a single structured JSON file
without parsing markdown.

**Storage**. `workspace/journal.json`:

```json
{
  "schema_version": 1,
  "generated_at": "2026-06-15T12:00:00Z",
  "chapters": [
    {
      "chapter_id": "2026-06-15-…",
      "schema_id": "journal-mcp-canonical-v1",
      "current_state": "closed",
      "opened_at": 1781502216733,
      "closed_at": 1781502370716,
      "events": [
        {
          "event_id": "01KV4WX2GXKDRYM5Z4Z9KXBXNS",
          "event_type": "open",
          "section_name": null,
          "payload": { … },
          "created_at": 1781502216733
        },
        …
      ]
    },
    …
  ]
}
```

**Lifecycle**.

- `mark_dirty(chapter_id)` — marks the on-disk file as stale; lazy rebuild
  on next `journal_projection_rebuild` (or eager on each mutation,
  configurable via debounce window)
- `rebuild_chapter(replay)` — re-serializes the affected chapter into the
  in-memory map, then re-writes the entire `journal.json` atomically
  (temp file + rename, same pattern as `FileProjection`)

**Implementation footprint**. ~80 LOC, no new dependencies (`serde_json`
already in tree).

**MCP tool surface**. No new tools.

---

### 2.3 `OutlineProjection` — Outline-MCP knowledge tree sync

**Purpose**. Project chapters as nodes in an Outline-MCP book, enabling
cross-project chapter search via the Outline knowledge graph and embedding
journal content into broader knowledge trees (rules / runbooks /
decision logs).

**Storage**. External — Outline-MCP backend (default: `~/.outline/`).

**Mapping**.

```
Outline book = configured target (e.g. "journal-<project_name>")
  └── root node (e.g. "Chapters")
        ├── <chapter_id-1> (one node per chapter)
        │     └── content: chapter heading + 5 required sections rendered
        ├── <chapter_id-2>
        └── …
```

**Lifecycle**.

- `mark_dirty(chapter_id)` — queues the chapter for sync; debounced batch
  push to Outline (default 5 s window)
- `rebuild_chapter(replay)` — `outline.node_query(slug=<chapter_id>)` → if
  exists, `node_update(content=…)`; else `node_create(parent=…, content=…)`
- chapter close → `current_state` field on the Outline node updates

**Configuration**.

```yaml
projections:
  outline:
    enabled: true
    book_slug: "journal-myproject"
    parent_node_path: "Chapters"
    debounce_ms: 5000
```

**MCP client wiring**. The Outline-MCP runs as a separate stdio MCP server.
v0.3.0 introduces a small `outline_client` module that:

1. Spawns the `outline-mcp` binary as a child process (re-using the same
   process for the lifetime of `JournalMcpServer`)
2. Routes `node_create` / `node_update` / `node_query` calls over stdio
   using the `rmcp` client primitives

Alternative implementation paths considered: HTTP API (unsupported by
Outline-MCP); CLI subprocess per call (high latency).

**Implementation footprint**. ~300 LOC (client wrapper + projection +
config) + `rmcp` client feature dependency.

**MCP tool surface**. No new tools on `journal-mcp` side; the Outline
node graph becomes queryable via the Outline-MCP tool set directly.

---

### 2.4 `MiniAppProjection` — mini-app cross-search row sync

**Purpose**. Project chapter metadata as rows in a mini-app table, enabling
SQL-style cross-table joins (e.g. chapter ↔ issue ↔ commit cross-references)
via the mini-app's filter API.

**Storage**. External — mini-app SQLite (default `~/.mini-app/journal_chapter/`).

**Mapping**.

```yaml
# mini-app schema for `journal_chapter` table
fields:
  - name: chapter_id
    type: string
    required: true
  - name: project_root
    type: string
    required: true
  - name: schema_id
    type: string
    required: true
  - name: current_state
    type: string
    required: true
  - name: opened_at
    type: integer
    required: true
  - name: closed_at
    type: integer
    required: false
  - name: decided_summary
    type: string
    required: false
  - name: issue_refs
    type: array
    required: false
    description: extracted from `Issues touched` section
```

**Lifecycle**.

- `mark_dirty(chapter_id)` — queues the chapter row for sync
- `rebuild_chapter(replay)` — extract metadata + `Decided` summary +
  `Issues touched` IDs → `mini-app.create` (or `update` if row exists,
  keyed by `chapter_id`)
- Issue ref extraction: regex `[a-f0-9]{8}-[a-f0-9]{4}-…` (mini-app UUID
  shape) on `Issues touched` section body; collect all matches into the
  `issue_refs` array

**Configuration**.

```yaml
projections:
  miniapp:
    enabled: true
    table_name: "journal_chapter"  # default
    project_label: "myproject"     # populates project_root field
    debounce_ms: 5000
```

**MCP client wiring**. Same `rmcp` client approach as `OutlineProjection`,
targeting `mini-app-mcp`.

**Implementation footprint**. ~250 LOC.

**MCP tool surface**. No new tools on `journal-mcp` side; rows queryable
via mini-app tool set.

---

### 2.5 `VectorProjection` — embedding-based semantic search

**Purpose**. Semantic search across all chapter bodies: "find chapters
where I decided about X" without exact keyword match. Highest functional
value of the five, but also the deepest dependency stack.

**Storage**. `sqlite-vec` virtual table in `.journal.db`:

```sql
CREATE VIRTUAL TABLE journal_vec USING vec0(
    chapter_id TEXT PRIMARY KEY,
    embedding FLOAT[384]  -- dimension matches the chosen model
);
```

**Embedding model selection** (decision tree):

| Path | Pros | Cons | Decision |
|---|---|---|---|
| (a) `candle` + bundled `all-MiniLM-L6-v2` (384-dim) | Local, no API key, Metal-accelerated on Apple Silicon | ~25 MB binary increase, slow first-run model fetch | **default**, behind feature flag |
| (b) OpenAI embedding API (`text-embedding-3-small`) | No model bundling, high quality | API key, network round-trip per chapter, paid | optional, via separate feature flag |
| (c) Configurable HTTP endpoint (Ollama / vLLM / SGLang local) | User-supplied local server | Requires user infra | optional, via env var |

v0.3.0 ships (a) as the default + (c) as the env-configured alternative.
(b) is out of scope (separate issue if requested).

**Lifecycle**.

- `mark_dirty(chapter_id)` — flags the embedding for re-computation
- `rebuild_chapter(replay)` — concatenate all section bodies → tokenize +
  embed → `INSERT OR REPLACE INTO journal_vec`

**New MCP tool**: `journal_semantic_search(query, project_root?, limit?)`:

```rust
pub struct JournalSemanticSearchParams {
    /// Natural-language query to embed and match against chapter embeddings.
    pub query: String,
    /// Optional per-call project_root override (same semantics as other tools).
    #[serde(default)]
    pub project_root: Option<String>,
    /// Maximum number of results to return (default 10).
    #[serde(default)]
    pub limit: Option<usize>,
}
```

Returns a JSON array of `{ chapter_id, score, heading }` sorted by cosine
similarity descending.

**Implementation footprint**. ~400 LOC (model loading + sqlite-vec wiring
+ new tool handler + 6 unit tests).

**MCP tool surface**. One new tool: `journal_semantic_search` (17th tool).

---

## 3. Configuration / attach mechanism

### 3.1 Default-attached set

| v0.2.0 | v0.3.0 |
|---|---|
| `FileProjection` | `FileProjection`, `FTS5Projection`, `JsonProjection` |

Rationale for `FTS5` + `Json` as defaults:

- **Internal-only** (no external service dependency)
- **Storage co-located** in `.journal.db` (FTS5) or
  `workspace/` (Json) — no new directories
- **Backward compat**: existing `journal_grep` callers see a silent
  speedup; new `journal.json` file is additive

Rationale for `Outline` / `MiniApp` / `Vector` as **opt-in**:

- External service / model dependency
- May not be desired in every consumer environment
- Configuration explicit via env var or config file

### 3.2 Configuration sources (priority order)

1. **Per-call** — `journal_projection_attach(name, config?)` MCP tool
2. **Environment** — `JOURNAL_PROJECTIONS=file,fts5,json,outline,miniapp,vector`
3. **Config file** — `<project_root>/.journal/projections.yaml` (optional)
4. **Default set** — `FileProjection + FTS5Projection + JsonProjection`

### 3.3 Per-projection config schema

`<project_root>/.journal/projections.yaml`:

```yaml
projections:
  file:
    enabled: true
    debounce_ms: 500
  fts5:
    enabled: true
  json:
    enabled: true
  outline:
    enabled: false
    book_slug: "journal-myproject"
    parent_node_path: "Chapters"
    debounce_ms: 5000
  miniapp:
    enabled: false
    table_name: "journal_chapter"
    project_label: "myproject"
    debounce_ms: 5000
  vector:
    enabled: false
    model: "all-MiniLM-L6-v2"
    dimension: 384
    http_endpoint: null  # set to use Ollama / vLLM instead of bundled model
```

---

## 4. Migration path

### 4.1 v0.2.0 → v0.3.0 (backward-compatible)

- Existing callers see no API surface change to v0.2.0 tools.
- Default `JournalMcpServer::new()` attaches three projections (file +
  FTS5 + json) instead of one (file).
- Existing `workspace/.journal.db` gets two new virtual tables on first
  v0.3.0 run (`journal_fts` + lazy `journal_vec` only if VectorProjection
  is enabled).
- A new `workspace/journal.json` file appears on first projection rebuild.
- Existing `workspace/journal.md` content is unchanged.

### 4.2 Rebuild-on-upgrade

On first v0.3.0 startup against a v0.2.0 database, the new projections
have empty state. The server emits an info-level log line indicating that
`journal_projection_rebuild` should be invoked once to populate them:

```
INFO journal-mcp: new projections detected (fts5, json) — call
     journal_projection_rebuild(name="fts5") and journal_projection_rebuild(name="json")
     to backfill.
```

Or, optionally, an auto-backfill on startup behind an env var:

```
JOURNAL_AUTO_BACKFILL=1
```

### 4.3 Downgrade safety

v0.3.0 → v0.2.0 downgrade:

- v0.2.0 ignores the `journal_fts` / `journal_vec` virtual tables (no schema
  conflict; FTS5 module reads as a normal table from older binaries).
- v0.2.0 ignores `workspace/journal.json` (no v0.2.0 code reads it).
- No data loss.

---

## 5. Test strategy

### 5.1 Per-projection integration tests

Each projection ships ≥5 integration tests covering:

1. **`mark_dirty` + `rebuild_chapter` round-trip** — open a chapter, append
   sections, close, rebuild, verify projection output matches expected
2. **Idempotency** — `rebuild_chapter` called twice with the same replay
   produces identical projection output
3. **Multi-chapter ordering** — three chapters, verify projection consumes
   them in the correct order (newest first for chapter_list-style projections)
4. **Empty / corrupt input** — projection handles empty body / missing
   section gracefully (no panic)
5. **External dependency mock** — for Outline / MiniApp projections, mock
   the MCP client to verify the wire format without requiring a live backend

### 5.2 FTS5-specific tests

- Tokenization: Japanese substring query matches correctly
- Backward compat: `journal_grep` with FTS5 attached returns same results
  as the v0.2.0 LIKE-based path (regression test)

### 5.3 Vector-specific tests

- Embedding determinism: same body → same vector (across multiple
  invocations within a single process)
- Cosine similarity sanity: nearly identical bodies score ≥0.95;
  unrelated bodies score ≤0.5

### 5.4 Configuration tests

- env var → default config → file config → per-call config priority
- Conflicting `enabled` flags resolve to the higher-priority source

---

## 6. Implementation order

The five projections are independent; recommended landing order is by
dependency depth + landing risk:

### Phase α: FTS5 (~1 day, ~150 LOC)

Lowest dependency surface, highest immediate ROI (replaces LIKE scan in
`journal_grep`).

### Phase β: Json (~0.5 day, ~80 LOC)

Trivial implementation, unlocks programmatic consumers.

### Phase γ: Outline (~1–2 days, ~300 LOC)

First external-MCP integration; establishes the `outline_client` pattern
that MiniApp will reuse.

### Phase δ: MiniApp (~1 day, ~250 LOC)

Reuses the rmcp client pattern from Outline; small incremental work once
the client wrapper is in place.

### Phase ε: Vector (~2–3 days, ~400 LOC)

Highest-effort projection; ships behind a feature flag to avoid forcing
the `candle` dependency on all users.

**Total**: ~5–7 days, ~1180 LOC, 5 commits + 5 mini-app sub-issues.

---

## 7. Open questions (resolve before implementation)

1. **VectorProjection default model**. `all-MiniLM-L6-v2` (384-dim,
   bundled with `candle`) vs. user-supplied HTTP endpoint. Proposal:
   ship (a) as default behind `--features vector-local`, (c) via env
   var. (b) OpenAI API path deferred to a separate issue.
2. **OutlineProjection node hierarchy depth**. Flat (all chapters under
   one parent) vs. hierarchical (year / month / chapter). Proposal:
   flat in v0.3.0; hierarchical via configurable `parent_node_path`
   template in a follow-up.
3. **Default-attached set decision**. Three (`file + fts5 + json`) vs.
   one (`file` — current). Proposal: three as default in v0.3.0; users
   who want the v0.2.0 surface can set `JOURNAL_PROJECTIONS=file`.
4. **MiniApp schema deployment**. The `journal_chapter` mini-app table
   schema YAML must be deployed before the projection can write to it.
   Proposal: ship the YAML in `crates/journal-mcp/embed/miniapp/` and
   add an auto-deploy step that calls `mini-app.schema_create` on first
   sync if the table is absent.
5. **CHANGELOG / release strategy**. v0.3.0 is a MINOR release (no
   BREAKING). The pending schema rename refactor on master HEAD
   (commit `a15d180`) should either be folded into v0.3.0 OR released
   separately as v1.0.0-rc1 first. Proposal: fold into v0.3.0 (single
   release, BREAKING + Added in one ship).

---

## 8. Dependencies summary

| Crate | New / Existing | Used by |
|---|---|---|
| `rusqlite` | existing, enable `bundled-full` feature for FTS5 | FTS5Projection |
| `serde_json` | existing | JsonProjection |
| `rmcp` (client feature) | existing crate, new feature flag | OutlineProjection, MiniAppProjection |
| `candle-core`, `candle-nn`, `candle-transformers` | new (vector-local feature) | VectorProjection |
| `hf-hub` | new (vector-local feature) | VectorProjection model fetch |
| `tokenizers` | new (vector-local feature) | VectorProjection |
| `sqlite-vec` | new (always) | VectorProjection |
| `atom_syndication` | not used in v0.3.0 (deferred) | (future AtomFeedProjection) |
| `octocrab` | not used in v0.3.0 (deferred) | (future GhProjection) |

---

## 9. Tracking

- **Master issue**: `cc-x/journal-mcp: v0.3.0 Projection Suite (FTS5 / Json /
  Outline / MiniApp / Vector)` — mini-app issue to be filed alongside this
  doc.
- **Sub-issues** (5):
  - α: `FTS5Projection — full-text search index`
  - β: `JsonProjection — machine-readable dump`
  - γ: `OutlineProjection — Outline-MCP knowledge tree sync`
  - δ: `MiniAppProjection — mini-app cross-search row sync`
  - ε: `VectorProjection — embedding-based semantic search +
       journal_semantic_search tool`
- **Topic branch**: `topic/v0.3-projection-suite` (this doc lands here)
- **Worktree**: `.worktrees/v0.3-projection-suite`

---

## 10. References

- `crates/journal-mcp-core/src/projection.rs` — sealed `JournalProjection`
  trait + `ProjectionError`
- `crates/journal-mcp-core/src/projection/file.rs` — `FileProjection`
  reference implementation
- `docs/design.md` §4 Architecture, §6 MCP tool wiring, §8 EventLog +
  FileProjection
- `docs/migration-guide.md` §Open issues — replaced limitations
- v0.2.0 commits: `53d48c6` per-call `project_root` override,
  `817c2aa` `journal_chapter_list` pagination
