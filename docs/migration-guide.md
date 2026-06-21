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

**Path resolution rules (literal-fixed, no fallback):**

Once `project_root` is resolved at startup, all server-managed paths are
derived by literal `join` from it. No glob, mtime fallback, or backup-file
auto-pickup logic exists:

- `<project_root>/workspace/.journal.db` (EventLog SQLite) — literal-fixed
- `<project_root>/workspace/.journal.db-wal` and `.journal.db-shm` (SQLite WAL companions) — literal-fixed
- `<project_root>/workspace/journal.md` (FileProjection) — literal-fixed
- `<project_root>/.journal/schemas` (project-local L2 schema dir) — literal-fixed

If a backup-suffixed file (e.g. `.journal.db.bak.YYYYMMDD`) is the only file
present, the server does **not** pick it up automatically; a fresh empty DB
is created at the literal path on next startup. Use `mcp__journal__journal_info()`
to inspect the resolved paths at runtime.

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
# Stop the MCP client first — Unix open file semantics: rename(2) does not invalidate the existing fd, so a running server keeps writing to the renamed file.
mv workspace/.journal.db        workspace/.journal.db.bak.YYYYMMDD
mv workspace/.journal.db-shm    workspace/.journal.db-shm.bak.YYYYMMDD
mv workspace/.journal.db-wal    workspace/.journal.db-wal.bak.YYYYMMDD
# Restart the MCP client (fresh empty DB will be created on next startup)
```

**Why stop the client first (Unix open file semantics):**

On Unix, `rename(2)` (and the equivalent `mv` shell command) updates the
directory entry but does **not** invalidate any existing open file descriptor
that was previously opened against the original path. The kernel identifies
open files by inode, and the inode follows the renamed entry. A running
journal-mcp server therefore keeps writing through its existing fd to the
now-renamed `.journal.db.bak.YYYYMMDD` file, while the literal path
`<project_root>/workspace/.journal.db` (see §2.2) is no longer touched by
anyone. Skipping the stop step leaves the server in a state where new
chapters land in the backup file rather than the fresh DB.

This is not a bug in the server; the path resolution is literal-fixed by
design (see §2.2). The mitigation is to follow the stop-before-mv SOP in
the code block above, or to call `mcp__journal__journal_info()` to verify
the resolved `db_path` before importing. On startup, the server also scans
the workspace dir for stale `.journal.db.bak.*` / `.journal.db-wal.bak.*` /
`.journal.db-shm.bak.*` files and emits a `tracing::warn!` line per match
as an early-detection signal that the stop-before-mv SOP may have been
skipped in a previous migration.

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

## Migration: existing workspace-placement projects (v0.2.x → v0.3.0)

v0.3.0 で FileProjection default output path が
`<project_root>/workspace/journal.md` から
`<project_root>/journal.md` (root) に変更されました。

### 影響範囲

既存 project で `workspace/journal.md` を canonical surface としていた場合、
v0.3.0 以降は default で `<project_root>/journal.md` に書き出されます。
`workspace/.journal.db` (EventLog SQLite) は変更されません。

### Migration option A: per-call `output_path` で workspace 配置を維持 (推奨)

`journal_projection_rebuild` 呼び出し時に `output_path="workspace/journal.md"` を指定:

```
mcp__journal__journal_projection_rebuild(
  name="file",
  output_path="workspace/journal.md"
)
```

`output_path` は **one-shot** のため、通常運用の `close_chapter` 自動 write は
default (root) に行きます。ongoing 書き込みを workspace に継続したい場合は
Option B を参照してください。

### Migration option B: 物理 mv で root 配置に移行

```bash
cd <project_root>
mv workspace/journal.md journal.md
```

v0.3.0 default と整合し、以降の `close_chapter` 自動 write も同じ path に着地します。

---

## Migration: v0.3.x → v0.4.0 (env-gated FileProjection)

v0.4.0 で以下が同時に変更されました (BREAKING):

1. **FileProjection 自動 attach 廃止** — default では `journal.md` がどこにも
   出力されません。`JOURNAL_FILE_ENABLE` env を set した時のみ attach されます
2. **PATH default 復帰** — env-enabled 時の default path は v0.3.0 root から
   `<project_root>/workspace/journal.md` (= v0.2.x 互換) に戻りました
3. **`JournalInfoResult.file_projection_path` が `Option<PathBuf>` 化** —
   attach 無し時は `null` を返します

動機: repo root に repo log を出す前提が「意図しない git add 事故 / publish
事故」 を量産しやすく、 repo log を repo に commit する文化はまだ一般的
ではないため、 default を「何も出さない (EventLog SoT のみ)」 に戻し、
出したい人だけ env で明示 opt-in する形に再構成しました。

### 1. v0.2.x からの直接 migration

v0.2.x で `workspace/journal.md` 運用していた場合、`.mcp.json` の env
ブロックに `JOURNAL_FILE_ENABLE=1` を追加するだけで v0.4.0 でも同じ
`workspace/journal.md` に出力されます (path default が v0.2.x と一致):

```json
{
  "mcpServers": {
    "journal": {
      "command": "journal-mcp",
      "env": {
        "JOURNAL_PROJECT_ROOT": "/path/to/project",
        "JOURNAL_FILE_ENABLE": "1"
      }
    }
  }
}
```

### 2. v0.3.x からの migration (root → workspace に戻す、 推奨)

v0.3.x で root の `journal.md` 運用していた場合、 v0.4.0 default
(workspace 配置) に切り替えるのが**推奨**です。 git add 事故 / publish
事故 risk を下げます:

```bash
# 1. 既存 root の journal.md は backup or 削除 (FileProjection は EventLog
#    から再生成可能なので削除して問題なし、 backup したい場合は mv)
cd <project_root>
rm journal.md   # または mv journal.md /tmp/journal.md.v0.3.0-backup

# 2. .mcp.json env に JOURNAL_FILE_ENABLE=1 を追加 (PATH 指定は不要、
#    default = workspace/journal.md が自動採用される)

# 3. server 再起動後、 次の close_chapter で workspace/journal.md が
#    生成される (または journal_projection_rebuild を明示呼び出し)
mcp__journal__journal_projection_rebuild(name="file")
```

### 3. v0.3.x からの migration (root 配置を継続する場合)

何らかの理由で root 配置を維持したい場合、 `JOURNAL_FILE_OUTPUT_PATH`
で明示します:

```json
{
  "mcpServers": {
    "journal": {
      "command": "journal-mcp",
      "env": {
        "JOURNAL_PROJECT_ROOT": "/path/to/project",
        "JOURNAL_FILE_ENABLE": "1",
        "JOURNAL_FILE_OUTPUT_PATH": "journal.md"
      }
    }
  }
}
```

注意: relative path は `JOURNAL_PROJECT_ROOT` 起点で resolve されます。
absolute path も指定可能 (e.g. `/var/log/myproject/journal.md`)。

### 4. File 出力をやめる選択肢 (新規 default)

v0.4.0 から、 file 出力なしで EventLog (`workspace/.journal.db`) のみで
運用するのが default です。 chapter 内容は MCP tool 経由で参照します:

- `mcp__journal__journal_tail(n=N)` — 末尾 N 章
- `mcp__journal__journal_grep(pattern, since?, until?)` — substring 検索
- `mcp__journal__journal_chapter_list(limit?, offset?)` — 全章一覧
- `mcp__journal__journal_progress_of(chapter_id)` — Progress 節

file 出力を完全に廃止する場合は `.mcp.json` から
`JOURNAL_FILE_ENABLE` / `JOURNAL_FILE_OUTPUT_PATH` を**削除**します
(unset = no attach = no file output)。 既存の `journal.md` file が残って
いる場合は手動で削除してください (server は touch しません)。

### 5. Env 詰めポイント (FAQ)

| 状況 | 挙動 |
|---|---|
| `JOURNAL_FILE_ENABLE` unset, `JOURNAL_FILE_OUTPUT_PATH` unset | 何も attach されない (新 default、 EventLog SoT のみ) |
| `JOURNAL_FILE_ENABLE=1`, `JOURNAL_FILE_OUTPUT_PATH` unset | `<project_root>/workspace/journal.md` に attach |
| `JOURNAL_FILE_ENABLE=1`, `JOURNAL_FILE_OUTPUT_PATH=foo/bar.md` | `<project_root>/foo/bar.md` に attach (relative) |
| `JOURNAL_FILE_ENABLE=1`, `JOURNAL_FILE_OUTPUT_PATH=/abs/path.md` | `/abs/path.md` に attach (absolute) |
| `JOURNAL_FILE_ENABLE` unset, `JOURNAL_FILE_OUTPUT_PATH=...` | **何も attach されない** (strict gate)、 startup に `tracing::warn!` 出力 |
| `JOURNAL_FILE_ENABLE=` (空文字列) | **attach される** (set/unset 判定は `var_os().is_some()`、 値は無視) |
| `JOURNAL_FILE_ENABLE=0` | **attach される** (値無視仕様。 disable したいなら env を unset) |

### 6. `journal_info()` の breaking change 対応

MCP 経由で `journal_info()` を呼んで `file_projection_path` を読んで
いる client は `Option<PathBuf>` (= JSON で `null` も来る) に対応する
必要があります:

```python
# 旧コード (v0.3.x 想定): path は常に string
fp_path = info["file_projection_path"]

# 新コード (v0.4.0+ 対応)
fp_path = info.get("file_projection_path")  # None なら no attach
if fp_path is None:
    print("FileProjection is not attached (EventLog only)")
else:
    print(f"FileProjection writes to: {fp_path}")
```

---

## See also

- [`docs/design.md`](design.md) — full design specification (EventLog, schema,
  FileProjection, state machine)
- [`README.md`](../README.md) — project overview + MCP tool list
- [`CHANGELOG.md`](../CHANGELOG.md) — version history (ST1–ST7 implementation
  log)
