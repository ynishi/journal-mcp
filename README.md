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

**WIP — design phase (v3)**. Implementation has not started.

See `docs/design.md` for the full design specification.

## Layout

```
journal-mcp/
├── Cargo.toml             # workspace
├── crates/
│   ├── journal/           # core library (JournalCore / ChapterSchema / EventLog / Projections)
│   └── journal-mcp/       # stdio MCP server binary
├── docs/
│   └── design.md          # design specification (this is the SoT)
└── LICENSE-MIT / LICENSE-APACHE
```

## License

MIT OR Apache-2.0
