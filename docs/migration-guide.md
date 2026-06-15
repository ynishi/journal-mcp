# Migration Guide: file-based `journal.md` → `journal-mcp` EventLog

> *Migration path for existing `workspace/journal.md` (or equivalent text-based
> project canonical history) into `journal-mcp` SQLite EventLog + FileProjection.*

This guide covers the one-time migration of an existing text-based
`journal.md` file (typically maintained via `>>` redirect append or manual
editing) into the `journal-mcp` event-sourced storage layer. After migration,
`workspace/journal.md` becomes a machine-rendered **FileProjection** of the
`workspace/.journal.db` EventLog, and all subsequent appends must go through
the `journal-mcp` MCP tool chain.

---

## 1. Overview

| Before migration | After migration |
|---|---|
| `workspace/journal.md` is the SoT (Source of Truth) | `workspace/.journal.db` (EventLog) is the SoT |
| Appends via `>>` redirect / Edit / Write | Appends via `mcp__journal__journal_open_chapter` → `journal_append_section` × N → `journal_close_chapter` |
| Schema validation is implicit / manual | Schema validation enforced by `close_chapter` (e.g. `journal-mcp-canonical-v1` requires 5 sections: Verified / Done / Decided / Not Done / Issues touched) |
| `journal.md` is hand-edited | `workspace/journal.md` is a machine-rendered FileProjection; direct edits are overwritten by the next `journal_projection_rebuild` |

The migration is **one-way and idempotent at the file level** (re-importing the
same file into a fresh DB produces the same chapter set), but `journal_import`
is **not idempotent against an existing DB** — chapter-ID collisions cause full
rollback. Always start from an empty `.journal.db` (see §6 Rollback).

---

## 2. Prerequisites

### 2.1 `journal-mcp` install

- `journal-mcp` v0.1.0 (ST7) or later. Binary should be on PATH (e.g.
  `cargo install --path crates/journal-mcp` produces `~/.cargo/bin/journal-mcp`).
- Verify: `which journal-mcp` resolves to the install path.

### 2.2 `JOURNAL_PROJECT_ROOT` resolution

The server resolves the project root **at startup**, via:

```
JOURNAL_PROJECT_ROOT env var
  ↓ (fallback if unset)
std::env::current_dir() (= the directory where the MCP client was launched)
```

Once started, the resolved `project_root` is **fixed for the lifetime of the
server process**. Per-call override is not currently supported (see
[Open Issues](#open-issues)). To migrate multiple projects, you must
restart the MCP client in each project root.

The server writes to:
- `<project_root>/workspace/.journal.db` (EventLog SQLite)
- `<project_root>/workspace/journal.md` (FileProjection)

### 2.3 MCP client registration

Add to `.mcp.json` (project-scope) or your user-global MCP config:

```json
{
  "mcpServers": {
    "journal": {
      "command": "journal-mcp",
      "args": []
    }
  }
}
```

For user-global (recommended for multi-project setups), use your MCP host's
user-scope config so the same entry applies across every project root.

Restart the MCP client (e.g. Claude Code restart) so the new server entry is
launched.

---

## 3. Pre-check (schema compliance verify)

Before importing, verify your `journal.md` is compliant with the target schema
(default: `journal-mcp-canonical-v1`). Non-compliant chapters cause `close_chapter`
validation to fail, which rolls back the entire `journal_import` batch.

### 3.1 `journal-mcp-canonical-v1` requirements

(See `docs/design.md` §5.1 for the full schema spec.)

- **H2 chapter heading form**: `## <date> — <name>`
  - `<date>` should be `YYYY-MM-DD` (the importer slugifies the entire heading
    for chapter_id derivation; missing date prefix can still be imported, but
    breaks downstream date-aware tooling)
- **5 required H3 sections per chapter** (each non-empty):
  - `### Verified`
  - `### Done`
  - `### Decided`
  - `### Not Done` (or `### 残課題` — both are accepted by the importer's H3
    matcher, but only `Not Done` matches the schema's `sections_present` list;
    chapters using only `### 残課題` may pass import but fail schema close
    validation)
  - `### Issues touched`
- Optional sections: `Cleanup pending`, `Rejected paths`, `Progress`, `Notes`

### 3.2 Detecting non-compliant chapters

A simple Python check that walks all H2 chapters and flags those missing
required sections or the date prefix:

```python
import re
from pathlib import Path

H2 = re.compile(r'^## ')
DATE = re.compile(r'^20[0-9]{2}-[0-9]{2}-[0-9]{2}')
VER = re.compile(r'^### Verified')
DON = re.compile(r'^### Done')
DEC = re.compile(r'^### Decided')
ND  = re.compile(r'^### (Not Done|残課題)')
IT  = re.compile(r'^### Issues touched')

p = Path("workspace/journal.md")
text = p.read_text(encoding='utf-8').splitlines()

chapters = []
cur = None
for line in text:
    if H2.match(line):
        if cur is not None:
            chapters.append(cur)
        cur = {'heading': line[3:].strip(), 'V':0, 'D':0, 'Dc':0, 'ND':0, 'IT':0}
    elif cur is not None:
        if VER.match(line): cur['V']=1
        elif DON.match(line): cur['D']=1
        elif DEC.match(line): cur['Dc']=1
        elif ND.match(line):  cur['ND']=1
        elif IT.match(line):  cur['IT']=1
if cur is not None:
    chapters.append(cur)

missing_5sec = sum(1 for c in chapters if (c['V']+c['D']+c['Dc']+c['ND']+c['IT']) < 5)
no_date     = sum(1 for c in chapters if not DATE.match(c['heading']))
status      = "PASS" if (missing_5sec == 0 and no_date == 0) else "FAIL"
print(f"[{status}] total={len(chapters)} missing_5sec={missing_5sec} no_date={no_date}")
```

### 3.3 Normalize non-compliant chapters (uniform stub)

If the pre-check returns `FAIL`, two options:

#### Option A — Uniform stub (recommended for legacy / silent-form chapters)

Append a uniform 5-section stub to each non-compliant chapter, preserving the
original prose body. The chapter's narrative intent stays in the original
paragraph; the stub satisfies schema validation:

```markdown
### Verified
- (schema-normalize stub) Original chapter was recorded in a silent / brief /
  aggregated form that omitted the required schema sections. Stub appended
  post-hoc to record the format only; the chapter's narrative intent stays
  in the original prose body above.

### Done
- (schema-normalize stub) Original actions are preserved in the original prose
  body. Stub-only.

### Decided
- (schema-normalize stub) Chapter's original register intent (silent / brief
  / aggregated) is preserved; journal-mcp-canonical-v1 schema's required sections are
  post-hoc supplemented. Why: `journal-mcp` migration step requires schema
  compliance.

### Not Done / 残課題
- (schema-normalize stub) No carry from this chapter.

### Issues touched
- (schema-normalize stub) No issue touched in this chapter.

(YYYY-MM-DD schema-normalize: 5-section stub appended post-hoc for journal-mcp migration)
```

For chapters missing the H2 date prefix, prepend an inferred date (from
adjacent chapters or git log):

```
## YYYY-MM-DD (date-inferred) — <original heading text>
```

#### Option B — Per-chapter manual rewrite

For high-value chapters whose original narrative is rich enough, manually
decompose the prose into the 5 sections. Higher effort, better long-term
quality.

#### Option C — Schema mix (advanced)

Open non-compliant chapters under `minimal-v1` schema (which has no required
sections) and the rest under `journal-mcp-canonical-v1`. Note: the importer applies a
single schema per `journal_import` call; schema mixing requires manual
`journal_open_chapter` invocations.

---

## 4. Backup (mandatory)

Always back up the source `journal.md` before migration:

```bash
cp workspace/journal.md workspace/journal.md.bak.YYYYMMDD
```

If an existing `.journal.db` is present (e.g. from a prior dogfood run),
apply the [Dogfood Reset SOP](design.md#dogfood-reset-sop):

```bash
# Stop the MCP client first (so SQLite handle is released)
mv workspace/.journal.db        workspace/.journal.db.bak.YYYYMMDD
mv workspace/.journal.db-shm    workspace/.journal.db-shm.bak.YYYYMMDD
mv workspace/.journal.db-wal    workspace/.journal.db-wal.bak.YYYYMMDD
# Restart the MCP client (fresh empty DB will be created on next startup)
```

---

## 5. Import execution

### 5.1 Launch in project root

```bash
cd /path/to/your-project
# Launch your MCP client (e.g. Claude Code) here so the MCP server
# resolves JOURNAL_PROJECT_ROOT = current_dir = your-project root
```

### 5.2 Verify empty DB

Via the MCP client:

```
mcp__journal__journal_chapter_list()
  → expect: []  (empty array)
```

If non-empty, apply the [Dogfood Reset SOP](#4-backup-mandatory) before
proceeding.

### 5.3 Verify schema availability

```
mcp__journal__journal_schema_list()
  → expect to include: "journal-mcp-canonical-v1"
```

### 5.4 Run import

```
mcp__journal__journal_import(path="workspace/journal.md")
  → returns: ["<chapter_id_1>", "<chapter_id_2>", ...]
  → length should equal the H2 chapter count in your source file
```

### 5.5 Verify import result

```
# Tail check (last chapter evident chain)
mcp__journal__journal_tail(n=1)
  → expect: 1-element array with events chain containing
    open + section_append × N + close

# Chapter count check
mcp__journal__journal_chapter_list()
  → expect: array length == H2 chapter count
  → expect: all chapters have current_state="closed"
```

### 5.6 Optional: rebuild FileProjection

If you want `workspace/journal.md` re-rendered from the EventLog (e.g. to
canonicalize section order):

```
mcp__journal__journal_projection_rebuild(name="file")
```

Note: this overwrites `workspace/journal.md`. If the file content differs from
the last write (hash-check mismatch), the original is auto-backed up as
`workspace/journal.md.bak.<epoch_ms>` (see [README](../README.md) §Key behaviour
changes introduced in ST7).

---

## 6. Rollback (on failure)

If `journal_import` fails or schema validation rejects a chapter:

1. **Stop the MCP client** (release SQLite handles)
2. **Delete the in-progress DB**:
   ```bash
   rm workspace/.journal.db workspace/.journal.db-shm workspace/.journal.db-wal
   ```
3. **Restore source from backup** (if needed):
   ```bash
   cp workspace/journal.md.bak.YYYYMMDD workspace/journal.md
   ```
4. **Identify the failing chapter** from the error message (the importer
   returns the first failing `chapter_id`)
5. **Fix the failing chapter** (apply §3.3 normalization)
6. **Restart the MCP client** and re-run §5

The import is fully transactional — partial state is impossible. Either all
chapters land in `closed` state, or the DB stays empty.

---

## 7. Post-migration: new append protocol

After migration, all chapter writes must go through the MCP tool chain:

```
mcp__journal__journal_open_chapter(
  name="YYYY-MM-DD — <task summary>",
  schema_id="journal-mcp-canonical-v1"
)
  → returns chapter_id

mcp__journal__journal_append_section(
  chapter_id=<id>,
  section_name="Verified",
  body="<text>"
)
  → repeat for Done, Decided, Not Done, Issues touched, (optional sections)

mcp__journal__journal_close_chapter(chapter_id=<id>)
  → schema validation enforces 5 required sections + non-empty
  → returns "ok" on success
```

### Anti-patterns to avoid

- **Direct edit of `workspace/journal.md`** — the FileProjection is
  machine-rendered; direct edits are overwritten by the next
  `journal_projection_rebuild` (with a `.bak.<epoch_ms>` backup as a safety
  net, but the canonical state stays in the EventLog).
- **`>>` redirect / `echo` / `cat` append to `workspace/journal.md`** — same as
  above, plus zero schema validation.
- **Direct SQLite writes to `.journal.db`** — bypasses event sourcing and
  invariant tracking; corrupts replay paths.

### Recommended tool wrappers

If your AI agent or skill previously used a recipe like `j-append` (text-based
append), wrap it as a fail-loud deprecation so callers are routed to the new
MCP tool chain visibly:

```bash
#!/usr/bin/env bash
echo "DEPRECATED: use mcp__journal__journal_open_chapter / append_section / close_chapter" >&2
exit 1
```

Silent backward-compat (the recipe still writing to `workspace/journal.md` via
`>>`) is **structurally unsafe**: the next `journal_projection_rebuild`
overwrites the appended content with the EventLog-derived render, losing the
append entirely. Fail-loud is the only safe deprecation.

---

## Open issues

The following limitations are known and tracked separately:

- ~~**No per-call `project_root` override**~~ — **implemented in [Unreleased]**
  (see CHANGELOG). All 16 tools now accept an optional `project_root: Option<String>`
  argument. When omitted, the startup-time `JOURNAL_PROJECT_ROOT` (or
  `current_dir()`) is used (backward-compatible). When supplied, the server
  lazily opens (or reuses a cached) `JournalCore` rooted at the given path
  and executes the call against it. Multi-project workflows no longer need
  per-project MCP client restarts.
- ~~**`journal_chapter_list` large response**~~ — **implemented in
  [Unreleased]** (see CHANGELOG). `journal_chapter_list` now accepts
  optional `limit: Option<usize>` and `offset: Option<usize>` parameters
  for pagination. When both are omitted, the full chapter list is returned
  (backward-compatible). For large projects (100+ chapters), page through
  with `limit=20, offset=0`, `offset=20`, etc. Newest chapters first;
  `offset >= total` yields an empty list (not an error).

---

## See also

- [`docs/design.md`](design.md) — full design specification (EventLog, schema,
  FileProjection, state machine)
- [`README.md`](../README.md) — project overview + MCP tool list
- [`CHANGELOG.md`](../CHANGELOG.md) — version history (ST1–ST7 implementation
  log)
