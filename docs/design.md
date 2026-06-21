# lds journal module — first-class 設計

date: 2026-06-13
author: しー + ytk

本設計の核:
- 本 module の本質要件は **「任意の Chapter State Machine + section schema を持てる generic 装置」**
- Verified / Done / Decided / Not Done / Issues touched の 5 section は **default schema の 1 つ** = `journal-mcp-canonical-v1` として配布、 任意の Journaling Entity 設計を Multi-Schema で並走可能
- `JournalCore` は ChapterSchema parser + state transition engine + section policy enforcer のみ、 section 名は core に hardcode しない
- default canonical 5 section 規律は schema registry の 1 entry (= `journal-mcp-canonical-v1`) として §5.1 に literal で完全 inline 済、 外部 rule 文書を本 module 内から cross-ref しない
- BP-4.4 Dendron schema.yml + BP-8.1 mdschema を主要採用
- MCP tool に schema 操作 (`journal_schema_load` / `journal_schema_list` / `journal_schema_show`) を含む

## 1. 目的

Project の **正史** (= 判断 + 検証の流れ、 「再開・中断・路線変更判断のための単一参照点」) を AI / Human 共通で読める形で物理化する。 本質要件は:

- 章 = State Machine instance (open → appending → closed の状態遷移を持つ entity)
- 章の構造 (どの section が必須 / どの順 / 各 section の append policy / close 条件) は **schema 駆動で宣言**
- core は schema parser + state transition engine + EventLog + Projection だけを持ち、 **具体的な section 名 (Verified / Done / Decided / …) は core に hardcode しない**

これにより、 任意の Journaling Entity 設計を Multi-Schema で並走できる:
- default canonical (Verified / Done / Decided / Not Done / Issues touched) は `journal-mcp-canonical-v1` schema として配布
- ADR-MADR (Context / Decision / Consequences) は `madr-v1` schema として配布
- lab notebook (Hypothesis / Method / Result / Discussion) は `lab-v1` schema として配布
- project ごとに schema 選択 / 上書き / 拡張可能

副次目的:
- `Write` tool 経由の「Read full → concat → Write」 SOP を構造除去
- EventLog SoT + 複数 Projection で SoT 分散を防ぐ
- mini-app (row CRUD) / Outline (tree knowledge) と並ぶ「時系列正史」 layer

## 1.5. 新設の必要性 (既存 tool との差分)

近隣 ecosystem (`~/projects/persona-journal` / `~/projects/cc-x/*-mcp` / `~/projects/persona-pack` / `~/projects/mlua-*`) を走査した結果、 本 module の primitive (= **project-scoped chapter state machine + section policy schema + append-only EventLog + 多 Projection**) を full に提供する既存 tool は存在しない。 最も近い analog は **persona-journal** だが、 entity 形状が異なる。

### 1.5.1 比較表

| tool | scope | entity 形状 | 状態遷移 | section schema | append-only invariant | 不採用理由 |
|---|---|---|---|---|---|---|
| **persona-journal** | persona-scoped (`persona_id` 束縛) | flat entry (1 row = 1 emo / 1 brief snapshot) + `kind` taxonomy | なし (entry は単独 atomic note) | kind config (`path_template` / `versioning` / `indexed` の entry-level config のみ) | 部分的 (`versioning=true` の kind で entry 単位 version chain) | entity 形状不一致。 project 正史は **章 = 複数 section を持つ compound state machine** であり、 entry 単独 row では「Verified / Done / Decided / Not Done / Issues touched が同 chapter 内で揃って初めて close 可能」 という transition 制約を表現できない。 persona-journal を thin wrapper する path は scope (persona vs project) も entity 形状 (flat vs compound) も両方破る |
| **mini-app-mcp** | table-scoped (schema.yaml 駆動) | flat row (CRUD 自由) | なし | field-level type + required のみ | **なし** (update / delete 自由) | append-only invariant 不在 = 「正史」 用途で SoT になれない。 row 上に章構造を被せても schema 駆動の section policy / state transition を表現できない |
| **outline-mcp** | tree-scoped (book / node hierarchy) | tree node (knowledge primitive) | なし | node property (`inject` 等) | なし (node update 自由) | tree shape ≠ time-series。 章の「open → appending → closed」 順序が tree 構造に乗らない。 ナレッジ tree と時系列正史は直交 layer |
| **task-mcp** | just recipe runner | recipe execution | なし | なし | なし | domain 不一致 (作業実行) |
| **agent-inspect-mcp** | read-only log analyzer | session / agent log slice | なし | なし | (read-only) | domain 不一致 (subagent log 解析、 書き込み不可) |
| **git-workflow-mcp / git-reader-mcp / git-fetcher-mcp** | git porcelain wrapper | git ref / commit | なし | なし | (git の immutability に依存) | git log は機械的 diff、 「判断 + 検証の narrative」 を構造化された section schema で保持できない |
| **python-exec-mcp / rest-e2e-mcp** | sandbox / e2e runner | execution result | なし | なし | なし | domain 不一致 |
| **persona-pack** | persona identity (prompt body + extra) | persona TOML | なし | TOML schema | 部分的 (history snapshot) | identity 専、 時系列 narrative 用途ではない |
| **mlua-* (probe / lshape / lspec / batteries / 等)** | Lua runtime / pkg / spec | Lua module / VM | — | — | — | domain 不一致 (Lua 開発支援) |

### 1.5.2 新 primitive の本質 (どの既存 tool でも代替不能な要素)

本 module 固有の primitive は以下 4 つの combination。 個別要素は他 tool に類似物があるが、 **4 つ揃った combination** はどこにも存在しない。

1. **Chapter = compound State Machine entity**: open → appending → closed の transition + section 集合の close 条件 (sections_present / sections_non_empty) 駆動。 persona-journal の flat entry も mini-app の flat row もこの compound entity を表現できない
2. **Schema-driven section policy**: append-only-chain / append-only-log / append-once / replace-forbidden の 4 種を section ごとに宣言。 schema.yaml は mini-app にあるが entry-level、 section policy は本 module 独自
3. **Multi-Schema 並走**: `journal-mcp-canonical-v1` / `madr-v1` / `minimal-v1` / 将来の `lab-v1` 等を同一 project 内に混在可能、 chapter ごとに `chapter_meta.schema_id` で固定。 任意の Journaling Entity 設計 (ADR / lab notebook / 自作流派) を receiver project が自由に選べる generic 装置
4. **Project-scoped canonical history**: mini-app issue (= 枝、 作業単位) / outline (= ナレッジ tree) / persona-journal (= persona 独白) のいずれとも **直交する time-series narrative layer**。 「判断 + 検証 + 再開」 の単一参照点として project 1 本に紐付く

### 1.5.3 配置判断 — standalone MCP 別立て (現時点) / lds 統合 (将来 path 保持)

近隣 ecosystem の各 MCP は **単一責務 1 binary** の設計 (mini-app = CRUD store / outline = tree KB / task = recipe runner / persona-journal = persona diary / agent-inspect = log analyzer)。 本 module も同等の独立 MCP として切る。

**将来 vision (deferred)**: lds (local-develop-server) は既に `git` / `recipe` / `sandbox` / `gh` crate を同梱する dev workflow 集約 daemon で、 ここに journaling + mini-app + outline + algocline 本体を統合できれば **完全な任意 LocalDevServer** (project workflow 全 primitive を 1 daemon 集約) になる。 session_start(root) で project root を共有 / orch Phase hook を in-process 連携 / 章 open ↔ mini-app issue ↔ outline node の 3 軸 cross-reference を同 process 内で解決、 等の連携効果が見込める。

**現時点で統合を選ばない理由 (責務境界 risk)**:
- 4 primitive (journaling / mini-app / outline / algocline) を同時に lds に集約すると、 責務境界の握りが甘い段階で内部 coupling が一気に進み共倒れ risk が高い
- 本 module は新規 primitive (chapter state machine + multi-schema) で運用実績ゼロ、 まず独立 binary で API / EventLog schema / Projection 安定性を dogfood 検証する必要がある
- mini-app / outline / persona-journal は既に独立 MCP として安定運用中、 これらを巻き込む再配置は本 module 安定後の別 issue

**現実解 (本設計の決定)**:
- 本 module は **standalone MCP `journal-mcp`** として別 repo / 別 binary で構築 (cc-x 系 sibling 命名規約 `<noun>-mcp` に揃える、 配置候補 `~/projects/cc-x/journal-mcp/`)
- API / Projection / Schema が dogfood で安定したら lds 統合を別 issue で再検討
- 統合 path は「lds 内 crate として move + MCP tool 配線を lds 側に統合」 の形で予約 (本設計の API 形状は in-process 直呼出し / MCP stdio 経由のどちらでも成立する form で凍結する)

## 2. 配置決定

**standalone MCP server `journal-mcp`** (別 repo `~/projects/cc-x/journal-mcp/`、 別 binary)。 lds への統合は将来別 issue (§1.5.3 参照)。

## 3. crate 構成

standalone repo `~/projects/cc-x/journal-mcp/` (cc-x 系 sibling と並列配置) として:

```
journal-mcp/
└── crates/
    ├── journal          ← core library (in-process API + 全 primitive)
    └── journal-mcp      ← stdio MCP server binary (journal を rmcp-tools で expose)
```

`journal-mcp-core` crate 内部:
- `JournalCore` (state transition engine + section policy enforcer)
- `ChapterSchema` (YAML parser + State Machine spec)
- `SchemaRegistry` (built-in schemas + project-local schemas)
- `EventLog` (canonical SoT、 SQLite 固定)
- `JournalProjection` trait (sealed) + 各 projection 実装

将来 lds 統合時は `crates/journal-mcp-core` を lds repo に move + lds 側で MCP tool 配線、 `journal-mcp` binary は deprecate or 並走の判断を別 issue で。 API 形状 (`JournalCore` 直呼出し可能 + MCP stdio 経由可能) は両 path で互換になる form に凍結する。

## 4. Architecture

```
caller (AI / Human)
   │ MCP stdio
   ▼
JournalMcpServer   session_start(root)  [将来 lds 統合時は LdsServer session を共有]
   │
   └── JournalCore
         │
         ├── SchemaRegistry (hierarchical resolve, BP-1.2)
         │     ├── L1 built-in: journal-mcp-canonical-v1 / madr-v1 / minimal-v1 (crate 同梱)
         │     ├── L2 project-local: {project_root}/.journal/schemas/*.yaml
         │     ├── resolve 順: L2 が同 schema_id を override > L1 fallback
         │     └── chapter は chapter_meta.schema_id で版を固定 (open 後の resolve は immutable)
         │
         ├── ChapterSchema (YAML spec)
         │     ├── states: [open, appending, closed, ...]
         │     ├── transitions: from / to / on(event) / requires(condition)
         │     ├── sections: name / required / append_policy / evidence_required / hooks
         │     └── close_conditions: sections_present / sections_non_empty / custom_rules
         │
         ├── JournalCore
         │     ├── chapter boundary (date + name)
         │     ├── append-only invariant (write API は append_* / close_* のみ)
         │     ├── state transition engine
         │     │     (現 state + event → 次 state + validation)
         │     └── section policy enforcer (append-only / correction-chain / replace-forbidden)
         │
         ├── EventLog                      ← canonical SoT
         │     ├── SQLite (workspace/.journal.db)
         │     ├── PRAGMA WAL + STRICT + foreign_keys=ON
         │     ├── schema: event_id (ULID) / stream_id (chapter) /
         │     │           event_type / payload / previous_id / created_at
         │     ├── chapter_meta table: chapter_id / schema_id / current_state / opened_at
         │     └── BEFORE UPDATE/DELETE trigger で RAISE(ABORT)
         │
         └── trait JournalProjection (sealed)
               │   自分の SoT を持たない (EventLog 全 replay で再構築可能)
               ├── FileProjection      workspace/journal.md (default schema の render)
               ├── OutlineProjection
               ├── MiniAppProjection
               └── GhProjection
```

設計思想 (本 v3 の核):

- **ChapterSchema = State Machine spec** が章の骨格を完全に決定
- **core は schema 中立**: 「Verified を必須」 とか core に書かない、 schema が宣言する
- **EventLog の event_type は schema 由来**: 'section_append:<section_name>' / 'state_transition:<from>→<to>' / 'close' 等、 schema が定義する vocabulary
- **Projection は schema 込みで render**: FileProjection は default で `journal-mcp-canonical-v1` の markdown layout、 madr-v1 / lab-v1 用には render template を schema 側に付ける

## 5. ChapterSchema (declarative YAML spec)

### 5.1 `journal-mcp-canonical-v1` (default canonical)

```yaml
schema_id: journal-mcp-canonical
version: 1
description: journal-mcp default canonical history schema

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
    to: appending
    on: append_progress
  - from: appending
    to: closed
    on: close_chapter
    requires:
      sections_present: [Verified, Done, Decided, "Not Done", "Issues touched"]
      sections_non_empty: [Verified, Done, Decided]

sections:
  Verified:
    required: true
    append_policy: append-only-chain   # 訂正は previous_id chain
    evidence_required: true             # 証跡 (file:line / URL / hash) 必須
    description: 何を / どこで / どう検証したか + 結果

  Done:
    required: true
    append_policy: append-only-chain
    description: 実施した修正 / commit hash

  Decided:
    required: true
    append_policy: append-only-chain
    description: 設計判断 + Why=構造原因
    hooks:
      - on_append:
          type: keyword_detect
          patterns: ["次 session", "持ち越し", "次回", "next session"]
          response: warn_carryover     # Q-6 持ち越し宣言 hint

  "Not Done":
    required: true
    append_policy: append-only-chain
    description: 持ち越し (AI 自走可能な残作業)

  "Issues touched":
    required: true
    append_policy: append-only-chain
    description: 関連 issue ID

  # option sections
  "Cleanup pending":
    required: false
    description: 不可逆 / 課金 / Human 判断領分の後始末

  "Rejected paths":
    required: false
    description: 試したが棄却した技術 path の証跡

  "Progress":
    required: false
    append_policy: append-only-log     # 時系列 step log、 chain なし
    description: 中断あり長尺 task の step log

  "Notes":
    required: false
    description: 技術 tip / 再発防止知見

render:
  file_projection:
    chapter_header: "## {date} — {name}"
    section_header: "### {section_name}"
    section_order: [Verified, Done, Decided, "Not Done", "Issues touched",
                    "Cleanup pending", "Rejected paths", Progress, Notes]
```

### 5.2 他 schema の例

**`madr-v1`** (ADR-MADR):
```yaml
schema_id: madr
version: 1
sections:
  Context: { required: true, append_policy: append-only-chain }
  Decision: { required: true, append_policy: append-only-chain }
  Consequences: { required: true, append_policy: append-only-chain }
  "Considered Options": { required: false }
  "Pros and Cons": { required: false }
transitions:
  - from: appending
    to: closed
    on: close_chapter
    requires:
      sections_present: [Context, Decision, Consequences]
```

**`minimal-v1`** (制約なし、 schema validation skip):
```yaml
schema_id: minimal
version: 1
sections: {}   # 必須なし
transitions:
  - from: appending
    to: closed
    on: close_chapter
    requires: {}    # 制約なしで close 可能
```

### 5.3 append_policy 一覧

| policy | 意味 |
|---|---|
| `append-only-chain` | 同 section に複数 row 追記、 `previous_id` で訂正 chain。 訂正は新 row append (削除なし) |
| `append-only-log` | 同 section に複数 row 追記、 時系列順、 chain なし (Progress 節想定) |
| `append-once` | 同 section に 1 row のみ、 2 回目以降は error |
| `replace-forbidden` | row 自体は 1 つ、 後から replace 試行を block |

## 6. MCP tool 配線

| tool | 用途 |
|---|---|
| `journal_open_chapter` | 新章 (date + name + schema_id) を open。 schema 既定値は project config |
| `journal_append_section` | 既存章に section append (schema の append_policy + hooks 適用) |
| `journal_append_progress` | Progress 節に step 1 行 append (schema が定義していれば) |
| `journal_close_chapter` | 章 close。 schema の close transition の requires を strict check |
| `journal_tail` | 末尾 N 章 fetch |
| `journal_grep` | pattern + scope 検索 |
| `journal_chapter_list` | 章一覧 (since filter)、 output form は table 形式 (`date` / `chapter_title` / `current_state` / `Decided 1 行 summary` / `link`) で返す (BP-2.5/5.3 Microsoft Decision Log table) |
| `journal_progress_of` | 特定章の Progress 節 read |
| `journal_open_chapters` | close 前の章一覧 |
| `journal_schema_load` | schema YAML を SchemaRegistry に登録 |
| `journal_schema_list` | 利用可能 schema 一覧 (built-in + project-local) |
| `journal_schema_show` | 指定 schema の spec 表示 |
| `journal_projection_attach` | projection を 1 つ attach |
| `journal_projection_detach` | projection を 1 つ detach |
| `journal_projection_rebuild` | 指定 projection を EventLog 全 replay で再構築 |

## 7. State Machine + Schema enforcement

`JournalCore` は schema 駆動で動く:

```rust
pub struct JournalCore {
    log: EventLog,
    registry: SchemaRegistry,
    projections: Vec<Box<dyn JournalProjection>>,
}

impl JournalCore {
    pub fn open_chapter(&mut self, name: &str, schema_id: &str) -> Result<ChapterId> {
        let schema = self.registry.get(schema_id)?;
        let initial_state = schema.initial_state();
        let id = self.log.append_chapter_open(name, schema_id, initial_state)?;
        Ok(id)
    }

    pub fn append_section(&mut self, id: ChapterId, name: &str, body: &str) -> Result<()> {
        let chapter = self.log.chapter_meta(id)?;
        let schema = self.registry.get(&chapter.schema_id)?;

        // 1. state transition check
        let next_state = schema.transition(&chapter.current_state, &Event::AppendSection)?;

        // 2. section policy check
        let section_spec = schema.section(name)?;
        match section_spec.append_policy {
            AppendPolicy::AppendOnce => {
                if self.log.section_count(id, name)? > 0 {
                    return Err(Error::AppendOncePolicy);
                }
            }
            _ => {}
        }

        // 3. hooks (keyword_detect 等)
        let warns = schema.run_hooks(name, body);

        // 4. log append (canonical SoT)
        self.log.append_section(id, name, body, next_state)?;

        // 5. projection rebuild (dirty marking + debounce)
        for p in &mut self.projections {
            p.mark_dirty(id)?;
        }

        Ok(warns)   // 持ち越し検知 hint 等を返す
    }

    pub fn close_chapter(&mut self, id: ChapterId) -> Result<()> {
        let chapter = self.log.chapter_meta(id)?;
        let schema = self.registry.get(&chapter.schema_id)?;
        let next_state = schema.transition(&chapter.current_state, &Event::CloseChapter)?;
        // ↑ transition の requires (sections_present / sections_non_empty) は schema 側でここで check
        self.log.append_close(id, next_state)?;
        for p in &mut self.projections {
            p.rebuild_chapter(&self.log.chapter(id)?)?;
        }
        Ok(())
    }
}
```

ポイント:
- **「必須 section」 を core に持たない**。 schema 側 `transitions[].requires.sections_present` で表現
- **state transition が失敗すれば error**。 schema が「open 直後の close 禁止」 等を表現可能
- **hooks は schema 側で宣言、 core は実行のみ**。 keyword_detect / regex_detect / external_validator (将来 plugin) 等の type を schema が指定

### 7.1 Typestate 補強 (BP-6.2、 JournalCore 内部限定)

公開 API (`JournalCore::append_section` 等) は trait object dispatch で動くが、 JournalCore 内部の chapter handle は typestate で compile-time guard を追加する。 schema 駆動の runtime check と組み合わせて二段 (compile + runtime) で「closed 章への append 試行」 等の bug を impl 段階で除去。

```rust
// 公開しない (pub(crate))
pub(crate) struct ChapterHandle<S: ChapterState> {
    id: ChapterId,
    schema: Arc<ChapterSchema>,
    _state: PhantomData<S>,
}

pub(crate) trait ChapterState {}
pub(crate) struct Open;
pub(crate) struct Appending;
pub(crate) struct Closed;
impl ChapterState for Open {}
impl ChapterState for Appending {}
impl ChapterState for Closed {}

// Closed には append_* method を impl しない = compile-time guard
impl ChapterHandle<Appending> {
    pub(crate) fn append_section(self, ...) -> Result<ChapterHandle<Appending>> { ... }
    pub(crate) fn close(self) -> Result<ChapterHandle<Closed>> { ... }  // schema requires runtime check
}
// ChapterHandle<Closed> は append_section / close を持たない型
```

外部 API は `JournalCore::append_section(id, ...)` のままで、 内部で `ChapterHandle<S>` の transition を回す。 trait object dispatch (`Box<dyn JournalProjection>`) と直交。

## 8. EventLog (canonical SoT) と FileProjection

### 8.1 EventLog schema (BP-3.1 sql-event-store + chapter_meta)

```sql
CREATE TABLE event_log (
    event_id     TEXT PRIMARY KEY,        -- ULID
    stream_id    TEXT NOT NULL,           -- chapter_id (date-slug)
    event_type   TEXT NOT NULL,           -- 'open' / 'section_append' / 'progress_append' / 'close' / 'import'
    section_name TEXT,                    -- section_append 時のみ
    payload      TEXT NOT NULL,           -- JSON
    previous_id  TEXT,                    -- correction chain
    created_at   INTEGER NOT NULL         -- unix epoch ms
) STRICT;

CREATE TABLE chapter_meta (
    chapter_id      TEXT PRIMARY KEY,
    schema_id       TEXT NOT NULL,        -- どの ChapterSchema instance か
    current_state   TEXT NOT NULL,
    opened_at       INTEGER NOT NULL,
    closed_at       INTEGER                -- closed 時のみ
) STRICT;

-- immutability guard (BP-3.2)
CREATE TRIGGER event_log_no_update BEFORE UPDATE ON event_log
    BEGIN SELECT RAISE(ABORT, 'event_log is append-only'); END;
CREATE TRIGGER event_log_no_delete BEFORE DELETE ON event_log
    BEGIN SELECT RAISE(ABORT, 'event_log is append-only'); END;

-- chapter_meta は state transition で UPDATE が走るので trigger なし、
-- ただし schema_id / opened_at は immutable check を schema 側 transition rule で担保
```

#### event_type: import (ST7 昇格、journal_import tool 本実装済)

1 event 内 payload に N chapter を畳む atomic batch wrapper。migration tool (`journal_import`) の EventLog 記録形式。詳細は §13 `journal_import` 昇格の記述を参照。

**Payload form (Option A 確定):**

```json
{
  "event_type": "import",
  "stream_id": "<migration_id (ULID)>",
  "section_name": null,
  "payload": {
    "source_path": "<input file path>",
    "source_hash": "<sha256 of input file>",
    "migration_epoch_ms": <unix ms>,
    "chapters": [
      {
        "chapter_id": "<deterministic from name>",
        "chapter_name": "<h2 literal>",
        "schema_id": "journal-mcp-canonical-v1",
        "sections": [
          { "section_name": "<h3>", "body": "<literal>" }
        ]
      }
    ]
  }
}
```

**Semantic:**

- 1 SQLite transaction で全 chapter を `closed` state に着地させる
- `chapter_id` collision は error rollback (skip / overwrite なし)
- 日常的な章追加は `journal_open_chapter` + `journal_append_section` + `journal_close_chapter` の連打で対応。`import` は migration 系用途に限定 (§13 参照)

**Replay 経路:**

既存の `replay_chapter` / `replay_until` は `import` event の payload を `chapter_open + section_append × N + chapter_close` 相当の virtual events 列に展開して処理する。ProjectionError / EventLog の replay API は `import` を透過的に扱う。

### 8.2 FileProjection

content-hash + dirty marking + debounce rebuild + atomic write の 4 段で構成 (BP-1.5/1.6/4.2):

- **content-hash**: chapter ごとに EventLog 由来 payload を hash 化、 既存 dump と一致なら skip
- **dirty marking**: `JournalCore::append_section` / `close_chapter` が該当 chapter_id を dirty marker に push
- **debounce rebuild**: 1 秒 debounce で chapter 単位 rebuild、 burst append (連続 section append) を 1 回の write に集約
- **atomic write**: tempfile (`workspace/.journal.md.tmp`) に書き出し → POSIX `rename(2)` で原子的差し替え

render template は active schema の `render.file_projection` から取得して chapter ごとに切替可能。 chapter_meta.schema_id で版固定されているため、 同 file 内に異 schema 章が混在しても各章は自分の render template で描画される。

## 9. 設計要件

設計時点で結論を出すべき項目を機能要件 / 非機能要件・invariant / 既定値・判断 の 3 軸で整理。 first cut でやらないもの・実装段階で実測が必要なものは §13 非 goal に分離。

### 9.1 機能要件

- 任意の `ChapterSchema` (YAML 宣言) を受けて state transition + section policy + hooks を enforce
- `EventLog` (SQLite) を canonical SoT、 複数 `JournalProjection` を read 側 dump として並走
- MCP tool 経由で chapter open / append / close / schema load・list・show / projection attach・detach・rebuild
- built-in schema として `journal-mcp-canonical-v1` / `madr-v1` / `minimal-v1` を crate 内 embed
- project-local schema を `{project_root}/.journal/schemas/*.yaml` から runtime load

### 9.2 非機能要件 / Invariant

- **append-only invariant**: trait に全文上書き / section 削除 / row 削除 API を expose しない
- **二層 immutability guard**: Rust trait sealed (BP-6.1) + SQLite trigger `BEFORE UPDATE/DELETE RAISE(ABORT)` (BP-3.2) の両方を強制
- **schema 中立**: core に section 名を hardcode しない (`Verified` / `Decided` 等は schema 側 literal、 core は schema spec を実行するエンジン)
- **format 互換性**: FileProjection は active schema の `render.file_projection` template に従い、 `journal-mcp-canonical-v1` 適用時は既存 `workspace/journal.md` format と完全互換
- **single-writer 前提**: multi-tenant / concurrent append は first cut 非 goal (§13)
- **atomic dump**: tempfile + POSIX rename で原子的書き換え (Windows 動作は §13 非 goal)
- **EventLog 完全性**: 全 Projection は EventLog 全 replay で再構築可能 (drift 概念を構造的に消す)

### 9.3 既定値・設計判断

| 項目 | 判断 |
|---|---|
| chapter_id | date-slug (外部表示、 例: `2026-06-13_journal-module-design`) + ULID (内部 PK、 BP-9) の 2 層 |
| dump 同期粒度 | dirty marking + 1 秒 debounce で chapter 単位 rebuild (BP-4.2) |
| schema 配布形式 | built-in は `rust-embed` で crate 同梱 (runtime download / central registry は非 goal) |
| schema versioning | `schema_id` に version 含める (`<name>-v<N>` 形式)、 v1 → v2 は別 schema として並走、 既存章は `chapter_meta.schema_id` で固定 |
| schema 拡張 (inheritance / overlay) | first cut 非 goal (§13 既出)、 必要時は別 issue で extend |
| 持ち越し宣言検知 | schema hooks の `keyword_detect` で declarative に表現、 tool は `warn_carryover` を response に return、 self-check 本体は caller (AI side) |
| `.journal.db*` の .gitignore | 既存 `workspace` Universal entry で wildcard cover、 新規 entry 追加不要 |
| SQLite PRAGMA | `journal_mode=WAL` + `synchronous=NORMAL` + `foreign_keys=ON` + `STRICT` table (BP-3.4) |
| Adapter 切替 migration | EventLog SoT + Projection rebuild API 1 本で代替、 専用 migration tool 不要 |
| 同時運用 drift | drift 概念自体が消える (両方 Projection で canonical = EventLog)、 dump staleness は dirty marking で検知 |
| lite mode 仕様 (open 中の空 section) | `open_chapter` から `close_chapter` の間は必須 section の row 数 0 を許容、 `close_chapter` 時のみ schema の `requires.sections_present` / `sections_non_empty` を strict check して fail なら transition 拒否 |
| 並行性モデル | single-writer 前提 + SQLite WAL (concurrent reader 1 writer)、 MCP session 内逐次処理、 並列 append は scope 外 (§13) |
| SchemaRegistry resolve | L1 built-in (crate 同梱) → L2 project-local (`.journal/schemas/`) の hierarchical resolve、 L2 が同 schema_id を override、 chapter は `chapter_meta.schema_id` で版固定 (BP-1.2) |
| ChapterHandle 内部表現 | JournalCore 内部の `ChapterHandle<S: ChapterState>` で typestate 適用 (`Open` / `Appending` / `Closed`)、 公開 API は影響なし (BP-6.2) |
| FileProjection auto-attach | v0.4.0 で廃止。 `JOURNAL_FILE_ENABLE` env set 時のみ attach、 unset 時は何も attach されない (EventLog SoT のみ運用が default)。 repo log を root に出す default の意図しない git add / publish 事故対策。 |
| `JOURNAL_FILE_OUTPUT_PATH` env | ENABLE set 時のみ有効 (strict gate: PATH 単独 set は startup warn + ignore)。 未指定 default = `<project_root>/workspace/journal.md` (v0.2.x 互換)、 relative path は `project_root` 起点、 absolute path は as-is。 詳細は `docs/migration-guide.md` §"v0.3.x → v0.4.0" 参照 |

## 10. 段階 (実装 step)

1. **standalone repo `journal-mcp` 起こす** — `~/projects/cc-x/journal-mcp/` に `crates/journal-mcp-core` (core library) + `crates/journal-mcp` (stdio MCP binary) の 2 crate workspace 構成
2. **crate `journal-mcp-core` 中身** — Core 型 + EventLog (SQLite + trigger + ULID + chapter_meta) + ChapterSchema parser (YAML) + SchemaRegistry + sealed JournalProjection trait
3. **built-in schema 同梱** — `journal-mcp-canonical-v1` / `madr-v1` / `minimal-v1` を crate 内 embed
4. **FileProjection 実装** — content-hash + chapter dirty marking + debounce rebuild + atomic write + schema 由来 render template
5. **MCP tool 配線** — `crates/journal-mcp` で `journal_*` tool 群 (schema 操作 3 本含む) を `#[tool_router]` で expose、 stdio transport
6. **dogfood** — local-develop-server / algocline / agent-profiles で `journal-mcp-canonical-v1` 運用、 持ち越し宣言 hint を warn return で AI 側 self-check トリガー
7. **他 Projection opt-in** — Outline / MiniApp / Gh
8. **schema 拡張機能** — schema inheritance / overlay (§13 非 goal からの昇格時、 別 issue)
9. **lds 統合検討** — API / Projection / Schema 安定後、 `crates/journal-mcp-core` を lds repo に move + lds 側 MCP tool 配線、 standalone binary deprecate 判断 (別 issue、 §1.5.3 参照)

## 11. 既存運用との位置関係

- default canonical (Verified / Done / Decided / Not Done / Issues touched + フォーマット / 配置 / 追記規律 / 禁止 / Adviser write 規律) は §5.1 の `journal-mcp-canonical-v1` schema YAML に literal で完全に inline 済。 本 module が走る project では schema YAML が運用 SoT、 外部 rule 文書はそれを参照する narrative layer に降りる
- mini-app issue (= 枝、 作業単位 + 受入条件) は本 module の scope 外、 役割分担は維持
- `tail` / `grep` / `progress_of` tool により journal.md 全 Read を構造的に不要化

## 12. 主要採用 BP (bp-survey.md 参照)

| BP | 採用箇所 |
|---|---|
| BP-1.4 Ann Arbor Architecture | §4 §5 dual-representation の theoretical reference |
| BP-1.5/1.6 memweave / sqlite-memory | §8.2 dump 戦略 (content-hash + dirty marking) |
| BP-3.1 sql-event-store schema | §8.1 EventLog schema |
| BP-3.2 SQLite BEFORE UPDATE/DELETE trigger | §8.1 DB 層 immutability guard |
| BP-3.4 PRAGMA WAL + STRICT + foreign_keys=ON | §8.1 SqliteAdapter default |
| BP-3.5 Event Sourcing projection rebuild | §6 `journal_projection_rebuild` |
| BP-4.2 memweave content-hash sync | §8.2 dirty marking + debounce |
| BP-4.4 Dendron schema.yml | §5 ChapterSchema declarative spec (主要採用に格上げ) |
| BP-6.1 Sealed trait pattern | §5 `JournalProjection: private::Sealed` |
| BP-8.1 mdschema declarative validator | §5 sections.required / append_policy 表現 |
| BP-8.6 Dendron template auto-insert | §6 `journal_open_chapter` 必須 section 空 placeholder (schema 駆動) |
| BP-9 ULID crate | §8.1 event_id 内部 PK |
| BP-2.1 MADR template | §5.2 `madr-v1` schema として直接同梱 |
| BP-2.5/5.3 Microsoft Decision Log table format | §6 `journal_chapter_list` output form (table + Decided 1 行 summary) |
| BP-6.2 Typestate pattern (Cliffle) | §7.1 JournalCore 内部 `ChapterHandle<S>` の compile-time state guard |
| BP-1.2 Claude Code hierarchical schema resolve | §4 / §9.3 SchemaRegistry の L1 built-in → L2 project-local resolve order |

### reference 群 (将来の implementation / 拡張 path、 first cut で impl しない)

| BP | 保持理由 |
|---|---|
| BP-1.3 Git Context Controller (arxiv 2508.00031) | §5 `close_chapter` = milestone marker の theoretical reference |
| BP-2.4 adrs (Rust 実装、 rusqlite + FTS5) | §10 Step 1 implementation reference (crate 構成の先例) |
| BP-2.2 adr-tools link/supersede concept | 将来 `journal_append_link(from, to, relation)` tool 追加時の予約 (今は append_section literal で済む) |
| BP-6.3 Linux kernel capability split (2 trait) | 将来「読み専用 Projection」 (Outline read-only 等) の path として保持 |

## 13. 非 goal (first cut scope 外)

別 issue 化済 / 別 issue 化候補。 本設計の Step 1-6 の completion 条件に含めない。

- mini-app issue / Outline book SoT の置き換えは行わない (役割分担維持: issue=枝、 journal=幹、 outline=ナレッジ tree)
- multi-user collab (concurrent append) は first cut goal 外、 single-writer 前提
- 既存 `workspace/journal.md` からの migration tool — **ST7 で昇格、journal_import tool 本実装済 (16 本目 MCP tool として登録、Option A 確定形)**。詳細は §8.1 `import` event_type 参照
- vector search (sqlite-vec / embedding) (将来 Projection の 1 つとして `VectorProjection` 追加可能)
- schema inheritance / overlay (Step 7、 first cut は単独 schema のみ)
- **Cross-platform Windows file IO**: first cut は POSIX (atomic rename) のみ対応。 Windows での代替 (file lock + copy 等) は別 issue
- **FTS5 / 全文検索 backend**: `journal_grep` は first cut で SQL `LIKE` ベース、 FTS5 移行は別 issue

### Dogfood Reset SOP (workspace/journal.md を消した後の復旧手順)

1. **事前確認**: `.journal.db` が存在する場合、DB が空 (章 0 件) であることを確認する。`import` は idempotent ではないため (chapter_id collision で error rollback)、既存 DB に章が残っているとエラーになる
2. **DB 削除**: `workspace/.journal.db` を削除する (空 DB に初期化するため)
3. **import 実行**: 元の markdown ファイル (version-controlled copy or backup) を `journal_import(path="<backup_journal.md>")` で取り込む
4. **projection 再生成**: `journal_projection_rebuild(name="file")` を明示呼び出して `workspace/journal.md` を再生成する

> **注意**: `import` は章の一括取り込みに使用する。日常的な章の追記には `journal_open_chapter` / `journal_append_section` / `journal_close_chapter` を使うこと。

